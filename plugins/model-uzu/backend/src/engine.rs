//! The uzu engine on its own thread: the loaded model, the resident conversations (one uzu state per
//! cache key, kept in GPU-visible unified memory), and the scheduler that runs requests on them.
//!
//! - **Per-conversation cache.** Each slot holds one conversation's state (attention KV, Gated
//!   DeltaNet recurrent state, speculator context) plus up to the fitted number of context
//!   checkpoints (least recently used evicted beyond it). Slots are not planned: one is made for
//!   a conversation while the shared pool has room for its fixed state (`conversation_tokens`) and
//!   its context, so how many are resident at once follows what they hold. A request's prefill checkpoints the reuse
//!   points the runtime found by rendering it (the end of the leading system block, the end of the
//!   history before its last message) and the point just before its last prompt token. A
//!   recurrent state cannot be truncated, so a request continues its key's state when the state is
//!   a prefix of the new prompt, rolls back to the longest checkpoint that is (later ones are then
//!   void), and starts cold otherwise. A caller whose every call ends in a per-call message (a
//!   stage, live data) thus resumes at the shared history instead of re-reading it. Conversations
//!   are kept resident within the fitted pool; the least recently used idle one is spilled to disk
//!   (hibernated) when room is needed, and restored from disk, without re-prefill, when its key
//!   returns.
//! - **Resident only while working.** A conversation with no request running or waiting is
//!   spilled to disk once it has been idle for `idle_spill` (the end of a turn: its calls follow
//!   each other closer than that), so memory holds the weights and the conversations being worked
//!   on, never every conversation ever opened. Its next request restores it from disk, without
//!   re-prefill. Metal memory is wired (never paged or compressed), so memory left holding idle
//!   conversations is memory the rest of the machine cannot get back. The spill files are bounded
//!   by `hibernate_limit` bytes; the oldest beyond it are deleted (that conversation starts cold).
//! - **Shared leading blocks.** Conversations of one project start with the same system block
//!   (system prompt, tools). The prefill that reaches a prompt's `shared` point (the end of that
//!   block) keeps a full copy of the state there (a seed, counted against the pool and dropped,
//!   least recently used first, before admission would wait for room); a request whose own state
//!   holds less of its prompt than a seed starts from a copy of it. A new conversation thus reads
//!   only what follows the block.
//! - **Scheduling.** uzu decodes one sequence per forward pass, so concurrent requests are
//!   interleaved: every step advances one request by one forward pass (or one 1024-token prefill
//!   chunk), choosing the highest priority class first and rotating within it. Foreground requests
//!   therefore preempt agent work at the next step. Requests the full pool cannot take (every
//!   resident conversation busy), or for a key whose slot is busy, queue by priority, first come
//!   first served within a class. Interleaving is concurrency, not parallelism: N running requests
//!   each decode at about 1/N of the speed of one alone.
//! - **Hibernation** writes a slot's full state to disk and frees it (vendor patch P1).
//! - **Images** (vendor patch P7): a request's images that are not in the reused prefix are
//!   encoded on the vision thread (`vision::worker`, its own engine context and command queue),
//!   which is started, with the tower, only when an image needs encoding and stopped after
//!   `vision_idle_unload` without one. Other conversations keep stepping meanwhile; the request
//!   prefills up to its first image and waits there for the embeddings. Loading the tower is
//!   preceded by a check of the device's actual headroom (idle conversations are hibernated to make
//!   room, else the request is refused); the static memory plan never reserves the tower. Image
//!   embeddings replace the pad tokens' rows and their MRoPE positions are planned by uzu. Prefix
//!   matching compares an identity of the tokens in which each image's run carries its digest.

use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::vision::VisionConfig;
use crate::vision::prompt::{ImageSpan, PromptImage, identity};
use crate::vision::worker::{self as vision_worker, VisionWorker};
use cc_protocol::contracts::model::{Priority, QueueDepth, SlotState, progress_phase, topics as model_topics};
use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokenizers::Tokenizer;
use uzu_engine::backends::{Backend, BackendSelection, DeviceFacts, select_backend};
use uzu_engine::engine::Engine;
use uzu_engine::engine::language_model::LanguageModel;
use uzu_engine::engine::language_model::grammar::{Grammar, GrammarCompiler, GrammarConfig, grammar_compiler};
use uzu_engine::engine::language_model::images::{ImageEmbedding, ImageInput};
use uzu_engine::engine::language_model::state::{LanguageModelState, LanguageModelStateSnapshot, SnapshotMode};
use uzu_engine::engine::language_model::stream::{LanguageModelStream, LanguageModelStreamOptions, SamplingMethod};

/// Prompt tokens per prefill step (uzu's own attention batch is 1024 tokens, so a step costs no
/// prefill efficiency; it bounds how long a foreground request waits behind another's prefill).
pub const PREFILL_CHUNK: usize = 1024;
/// Seconds a conversation stays resident after its last request (see "Resident only while working").
pub const DEFAULT_IDLE_SPILL_S: u64 = 30;
/// Bytes the spill files may take on disk by default.
pub const DEFAULT_HIBERNATE_LIMIT: u64 = 32 << 30;
/// The longest a turn's hold keeps a conversation resident without being released (a holder
/// that went away does not pin memory for good).
pub const MAX_HOLD: std::time::Duration = std::time::Duration::from_secs(4 * 60 * 60);
/// Pool cells reserved for a request's generation when the caller set no `max_tokens`. Admission
/// accounting only; it never limits the output.
pub const GENERATION_RESERVE: u64 = 8192;
/// Penalty window of a presence penalty (generated tokens only).
pub const PRESENCE_WINDOW: u32 = 4096;
/// Penalty window of a repetition penalty (the state's last tokens included, uzu's behaviour).
pub const REPETITION_WINDOW: u32 = 64;
/// File header of a hibernated conversation.
const HIBERNATION_MAGIC: &[u8; 8] = b"CCUZUHB1";
/// A request that reads at least this many prompt tokens is logged with what it reused.
const LONG_PREFILL_TOKENS: usize = 4096;

pub type Publisher = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// What the engine needs to know at load.
#[derive(Clone)]
pub struct EngineConfig {
    pub model_dir: PathBuf,
    /// Tokens one conversation may hold (the shared pool, at most the native context).
    pub state_capacity: u32,
    /// Tokens of the shared pool across resident conversations.
    pub pool_tokens: u64,
    /// An operator cap on resident conversations (None: as many as the pool holds).
    pub max_resident: Option<usize>,
    /// Pool tokens one resident conversation's fixed state stands for (the fit's
    /// `conversation_tokens`): charged while it holds a state, so resident conversations and their
    /// contexts share one pool.
    pub conversation_tokens: u64,
    /// Draft with the model's speculator (when it has one).
    pub speculation: bool,
    pub prefill_chunk: usize,
    /// Where hibernated conversations are written.
    pub hibernate_dir: PathBuf,
    /// How long a conversation stays resident after its last request ended before it is spilled to
    /// disk. Zero: kept until the pool needs the room.
    pub idle_spill: std::time::Duration,
    /// Bytes the spill files may take on disk; the oldest beyond it are deleted. Zero: unbounded.
    pub hibernate_limit: u64,
    /// Context checkpoints kept per conversation (the fit's `checkpoints`).
    pub checkpoints: usize,
    /// Identity of the loaded weights; a hibernated state restores only into the same.
    pub identity: String,
    /// The vision tower's directory (`vision/` of the model directory) when image input is enabled.
    pub vision_dir: Option<PathBuf>,
    /// Pixel budget per image.
    pub vision_max_pixels: u64,
    /// How long the vision tower stays loaded after its last encode.
    pub vision_idle_unload: std::time::Duration,
    /// The memory the plan fits in (bytes; the fit's budget). The vision tower loads only when the
    /// device's allocation plus the tower and its scratch stays within it. 0: unknown, not checked.
    pub memory_budget: Arc<AtomicU64>,
    /// KV pool bytes per token (the plan's): while the tower is resident, admission counts it as
    /// this many pool tokens, so growing conversations make room for it instead of overcommitting.
    pub bytes_per_token: u64,
}

/// Image embeddings kept in RAM by image and grid, so an image that left the prefix cache is not
/// encoded again (bounded; cleared when the vision tower unloads).
const EMBEDDING_CACHE_BYTES: usize = 256 << 20;

/// Sampling of one request, mapped from the contract (see `runtime.rs`).
#[derive(Debug, Clone, PartialEq)]
pub struct SamplingPlan {
    pub method: SamplingMethod,
    pub seed: u64,
    pub penalty_window_includes_state: bool,
    /// Penalties need flat (non-speculative) batches in uzu's sampler.
    pub allows_speculation: bool,
}

/// A reasoning budget: once the open reasoning block (the generation starts inside `<think>`) has
/// produced `tokens` tokens without closing, the engine feeds `close` (the model's own closing
/// sentence and `</think>`) as input and the model goes on with its answer. This is the thinking
/// budget Qwen documents for its reasoning models; the reasoning is kept, only its end is forced.
#[derive(Debug, Clone, PartialEq)]
pub struct ThinkBudget {
    pub tokens: u32,
    /// The token sequence that closes reasoning (`</think>`); seeing it ends the budget.
    pub end: Vec<u64>,
    /// What is fed when the budget is reached (contains `end`).
    pub close: Vec<u64>,
}

// One per request, moved once into the worker: the size difference to `Prewarm` costs nothing.
#[allow(clippy::large_enum_variant)]
pub enum Work {
    Generate {
        max_tokens: Option<u32>,
        sampling: SamplingPlan,
        grammar: Option<GrammarConfig>,
        /// Engage the grammar only after these tokens were generated (end of thinking).
        grammar_trigger: Option<Vec<u64>>,
        /// End a reasoning block that reaches its budget (Qwen's thinking budget).
        think_budget: Option<ThinkBudget>,
    },
    /// Prefill the prompt (all but its last token) without generating.
    Prewarm,
}

pub struct Request {
    /// Numbered by the engine when it receives the request (any value may be sent).
    pub id: u64,
    pub key: String,
    pub prompt: Vec<u64>,
    /// Prompt lengths, besides `prompt.len() - 1`, at which the prefill keeps a context checkpoint:
    /// reuse points shared with the conversation's later requests (see `runtime::Runtime::render`).
    pub checkpoints: Vec<usize>,
    /// The reuse point that ends the prompt's leading system block, when it has one: the same for
    /// every conversation that starts with that block, so the prefill keeps a copy of it there
    /// (a [`Seed`]) from which other conversations start.
    pub shared: Option<usize>,
    /// A prewarm of the leading block alone (`shared` is set): the prefill stops there, keeps the
    /// copy, and keeps no conversation under `key` (the conversation it is for does not exist yet).
    pub shared_only: bool,
    /// A prewarm that runs only while no other conversation is served: it waits while another
    /// conversation's request runs or waits, and such a request stops it at its next step.
    pub idle_only: bool,
    /// Images in the prompt (their pad-token runs), in order.
    pub images: Vec<PromptImage>,
    /// The vision configuration (present when `images` is not empty).
    pub vision: Option<Arc<VisionConfig>>,
    pub priority: Priority,
    pub work: Work,
    pub events: tokio::sync::mpsc::UnboundedSender<Event>,
    /// Set by the caller to end the request (cancel, stop string, repetition, grammar complete).
    pub stop: Arc<AtomicBool>,
    pub arrival: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Started { slot: u32, queue_ms: f64, cached_tokens: u32, prompt_tokens: u32 },
    Token(u32),
    Done(Outcome),
    Failed(Failure),
}

/// Why a request failed, by kind (mapped to protocol error codes by the runtime).
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// The request cannot run on this load (e.g. a prompt larger than a conversation may hold).
    Invalid(String),
    /// The engine is shutting down or switching; the request may be retried.
    Unavailable(String),
    Cancelled,
    /// The engine failed while running the request.
    Engine(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Finish {
    /// A stop token.
    Stop,
    /// `max_tokens` or the conversation's context capacity.
    Length,
    /// The caller set the stop flag.
    Stopped,
    /// Prewarm completed.
    Prefilled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub finish: Finish,
    pub prompt_tokens: u32,
    pub cached_tokens: u32,
    pub completion_tokens: u32,
    pub queue_ms: f64,
    pub prefill_ms: f64,
    pub decode_ms: f64,
    pub draft_total: u32,
    pub draft_accepted: u32,
    pub slot: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hibernated {
    pub key: String,
    pub file: PathBuf,
    pub n_tokens: u32,
    pub bytes: u64,
    pub identity: String,
}

/// An image a running request feeds: its first token's index in the prompt and its embeddings
/// (None while the vision thread encodes it).
struct ActiveImage {
    start: usize,
    grid: (u32, u32),
    key: String,
    rows: Option<Arc<[u16]>>,
}

impl ActiveImage {
    fn end(&self) -> usize {
        self.start + (self.grid.0 * self.grid.1) as usize
    }
}

/// An image's embedding cache key and its rows (None while it is encoded).
type ImageRows = (String, Option<Arc<[u16]>>);

/// The vision thread while the tower is loaded (or loading).
struct VisionRuntime {
    worker: VisionWorker,
    /// Last job submitted or answered (the idle period runs from here).
    last_used: Instant,
    /// Device bytes the tower takes (its configuration's estimate until the load is measured).
    resident_bytes: u64,
    /// The device's allocation when the thread started (the load's measured cost is relative to it).
    allocated_before: Option<u64>,
    loaded: bool,
}

/// The answer to `model.hibernate`: the record, or (error code, message).
pub type HibernateReply = tokio::sync::oneshot::Sender<Result<Value, (String, String)>>;

pub enum Command {
    Run(Box<Request>),
    Hibernate { key: String, reply: HibernateReply },
    /// Keep a conversation resident while its turn runs (`hold`), or let it go: a held one is not
    /// spilled for being idle (it still can be when the pool needs the room).
    Hold { key: String, hold: bool },
    /// Drop a conversation for good (its session was deleted): its requests stop, its state and
    /// checkpoints go, and so does its file on disk.
    Forget(String),
    /// Hibernate every idle conversation (before a swap or shutdown); replies with the records.
    HibernateAll { reply: tokio::sync::oneshot::Sender<Vec<Hibernated>> },
    /// Adopt conversations a predecessor hibernated.
    Adopt(Vec<Hibernated>),
    /// A message of the vision thread.
    Vision(vision_worker::Message),
    Shutdown,
}

/// What `model.slots` reports, maintained by the worker.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub slots: Vec<SlotState>,
    pub queued: QueueDepth,
    pub busy: u32,
    pub pool_tokens: u64,
    pub reserved_tokens: u64,
    pub hibernated: Vec<String>,
    /// The vision tower is resident.
    pub vision_loaded: bool,
}

/// Facts about the loaded engine.
#[derive(Debug, Clone, Serialize)]
pub struct Loaded {
    pub load_ms: f64,
    pub device: DeviceFactsJson,
    pub stop_token_ids: Vec<u64>,
    pub has_speculator: bool,
    pub max_context_length: Option<u32>,
    pub default_temperature: Option<f32>,
    pub peak_memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DeviceFactsJson {
    pub name: Option<String>,
    pub gpu_cores: Option<u32>,
    pub gpu_family: Option<String>,
    pub matrix_units: Option<bool>,
    pub recommended_working_set_bytes: Option<u64>,
    pub allocated_bytes: Option<u64>,
    pub sparse_buffers: bool,
}

impl From<DeviceFacts> for DeviceFactsJson {
    fn from(facts: DeviceFacts) -> Self {
        Self {
            name: facts.name,
            gpu_cores: facts.gpu_cores,
            gpu_family: facts.gpu_family,
            matrix_units: facts.matrix_units,
            recommended_working_set_bytes: facts.recommended_working_set_bytes,
            allocated_bytes: facts.allocated_bytes,
            sparse_buffers: facts.sparse_buffers,
        }
    }
}

/// Hibernation and restore costs, reported to the runtime's metrics.
pub trait CostSink: Send + Sync {
    fn saved(&self, tokens: u64, bytes: u64, ms: f64);
    fn restored(&self, tokens: u64, bytes: u64, ms: f64);
}

/// The running engine (the worker thread and its channel).
pub struct EngineHandle {
    tx: Mutex<mpsc::Sender<Command>>,
    pub view: Arc<Mutex<View>>,
    pub tokenizer: Arc<Tokenizer>,
    pub loaded: Loaded,
    pub backend: &'static str,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Device facts refreshed by the worker after every request.
    pub device: Arc<Mutex<DeviceFactsJson>>,
    /// Device facts measured now (allocated bytes change as conversations come and go).
    live_facts: Box<dyn Fn() -> DeviceFactsJson + Send + Sync>,
}

impl EngineHandle {
    pub fn send(&self, command: Command) -> Result<(), String> {
        let tx = self.tx.lock().map_err(|_| "engine channel poisoned".to_owned())?;
        tx.send(command).map_err(|_| "the engine thread has stopped".to_owned())
    }

    pub fn device_now(&self) -> DeviceFactsJson {
        (self.live_facts)()
    }

    pub fn view(&self) -> View {
        self.view.lock().map(|view| view.clone()).unwrap_or_default()
    }

    /// Stop the worker and wait for it (the model is released when it returns). Dropping the
    /// handle stops the worker too (without waiting).
    pub fn shutdown(&self) {
        let _ = self.send(Command::Shutdown);
        if let Ok(mut thread) = self.thread.lock()
            && let Some(thread) = thread.take()
        {
            let _ = thread.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        // The worker holds a sender of its own channel (for the vision thread), so the channel
        // never disconnects: ask it to stop.
        let _ = self.send(Command::Shutdown);
    }
}

/// Load the model on a new worker thread (blocking until loaded).
pub fn start(config: EngineConfig, publisher: Publisher, costs: Arc<dyn CostSink>) -> Result<EngineHandle, String> {
    struct Start {
        config: EngineConfig,
        publisher: Publisher,
        costs: Arc<dyn CostSink>,
    }
    impl BackendSelection for Start {
        type Output = EngineHandle;
        type Error = String;
        fn select<B: Backend>(self) -> Result<EngineHandle, String> {
            spawn_worker::<B>(self.config, self.publisher, self.costs)
        }
    }
    select_backend(Start { config, publisher, costs }, "unknown uzu backend (UZU_BACKEND)".to_owned())
}

fn spawn_worker<B: Backend>(
    config: EngineConfig,
    publisher: Publisher,
    costs: Arc<dyn CostSink>,
) -> Result<EngineHandle, String> {
    let (tx, rx) = mpsc::channel::<Command>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(Arc<Tokenizer>, Loaded, Arc<Engine<B>>), String>>();
    let view = Arc::new(Mutex::new(View::default()));
    let device = Arc::new(Mutex::new(DeviceFactsJson::default()));
    let worker_view = view.clone();
    let worker_device = device.clone();
    let worker_commands = tx.clone();
    let thread = std::thread::Builder::new()
        .name("uzu-engine".into())
        .spawn(move || {
            let started = Instant::now();
            let loaded = (|| -> Result<(Arc<Engine<B>>, LanguageModel<B>), String> {
                let engine = Engine::<B>::new().map_err(|error| format!("cannot start uzu: {error}"))?;
                let model = engine
                    .load_language_model(&config.model_dir)
                    .map_err(|error| format!("cannot load {}: {error}", config.model_dir.display()))?;
                Ok((engine, model))
            })();
            let (engine, model) = match loaded {
                Ok(pair) => pair,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            let tokenizer = model.tokenizer().clone();
            let generation = model.generation_config().clone();
            let facts: DeviceFactsJson = engine.device_facts().into();
            let loaded = Loaded {
                load_ms: started.elapsed().as_secs_f64() * 1000.0,
                device: facts.clone(),
                stop_token_ids: generation.stop_token_ids.to_vec(),
                has_speculator: config.model_dir.join("speculator").join("config.json").exists(),
                max_context_length: model.max_context_length(),
                default_temperature: generation.temperature,
                peak_memory_bytes: engine.peak_memory_usage().map(|bytes| bytes as u64),
            };
            if let Ok(mut slot) = worker_device.lock() {
                *slot = facts;
            }
            let stop_ids: Vec<u32> = generation.stop_token_ids.iter().map(|id| *id as u32).collect();
            let compiler = match model_vocab(&config.model_dir)
                .and_then(|vocab| {
                    let json = std::fs::read(config.model_dir.join("tokenizer.json")).map_err(|error| error.to_string())?;
                    grammar_compiler(&json, &stop_ids, vocab).map_err(|error| error.to_string())
                }) {
                Ok(compiler) => compiler,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("cannot prepare grammar support: {error}")));
                    return;
                }
            };
            let _ = ready_tx.send(Ok((tokenizer, loaded, engine.clone())));
            let mut worker = Worker::<B>::new(config, engine, Arc::new(model), compiler, rx, worker_commands, worker_view, worker_device, publisher, costs);
            worker.run();
        })
        .map_err(|error| format!("cannot start the engine thread: {error}"))?;
    match ready_rx.recv() {
        Ok(Ok((tokenizer, loaded, engine))) => Ok(EngineHandle {
            tx: Mutex::new(tx),
            view,
            tokenizer,
            loaded,
            backend: B::NAME,
            thread: Mutex::new(Some(thread)),
            device,
            live_facts: Box::new(move || engine.device_facts().into()),
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err("the engine thread ended during load".into())
        }
    }
}

/// A context checkpoint of a slot's state and when it was last taken or restored.
struct Checkpoint<B: Backend> {
    snapshot: LanguageModelStateSnapshot<B>,
    last_used: Instant,
}

/// A leading system block shared across conversations: a full copy of a state holding exactly it
/// (attention rows included, unlike a [`Checkpoint`]), so a conversation that starts with the same
/// tokens starts from it instead of re-reading it. Its bytes count against the pool.
struct Seed<B: Backend> {
    snapshot: LanguageModelStateSnapshot<B>,
    /// Pool tokens it stands for (its bytes over the plan's bytes per token).
    tokens: u64,
    last_used: Instant,
}

/// Leading blocks kept at once (the chat and agent system blocks of the open project).
const SEEDS: usize = 2;

/// One resident conversation.
struct Slot<B: Backend> {
    key: Option<String>,
    /// Boxed: a live stream points into it (see [`Active`]).
    state: Option<Box<LanguageModelState<B>>>,
    /// Checkpoints of `state`, each a prefix of its tokens (a rollback voids the longer ones).
    checkpoints: Vec<Checkpoint<B>>,
    /// Tokens the state holds (kept here so it can be read while a stream borrows the state).
    n_tokens: u32,
    last_used: Instant,
    busy: Option<Priority>,
    /// Images in the state's tokens (for prefix matching).
    images: Vec<ImageSpan>,
    /// The length of the leading system block, whose checkpoint is never evicted: a rewrite of the
    /// conversation then reuses at least that much (seen: a 220k prompt read from zero because the
    /// block's checkpoint had been evicted as least recently used).
    pinned: Option<usize>,
}

enum Phase<B: Backend> {
    /// Prompt tokens `[next, end)` remain to be prefilled before generation starts.
    Prefill { next: usize },
    Generate(Box<Generation<B>>),
}

/// A request running on a slot.
struct Active<B: Backend> {
    slot: usize,
    request: Box<Request>,
    phase: Phase<B>,
    cached: u32,
    queue_ms: f64,
    admitted: Instant,
    reserve: u64,
    first_token: Option<Instant>,
    last_token: Option<Instant>,
    generated: u32,
    /// Draft tokens proposed and accepted (speculative decoding), summed over the request's streams.
    draft: (u32, u32),
    /// Tokens emitted so far (with any fed closing tokens), to rebuild the input of a new stream.
    emitted: Vec<u64>,
    /// Reasoning tokens produced in the open reasoning block, while a budget applies.
    thinking: Option<u32>,
    /// Images still to be fed during prefill.
    images: Vec<ActiveImage>,
    /// Throttle of its `model.progress` events.
    reporter: crate::progress::Reporter,
}

/// A live uzu stream. It borrows the model and its slot's state, both of which the worker keeps
/// alive and untouched while the stream exists (invariant of the `'static` below): the model `Arc`
/// outlives every `Active` (the worker drops all of them first), and a slot's state `Box` is not
/// accessed, moved or dropped while its slot is busy.
struct Generation<B: Backend> {
    stream: LanguageModelStream<'static, B>,
    max_tokens: Option<u32>,
    stop_tokens: Vec<u64>,
}

/// How long the engine must have served nothing else before an idle-only generation may take
/// room the way any request does (spill or drop an idle conversation, drop a shared block): until
/// then it uses only free room, so work under way and its caches are never touched.
const IDLE_ROOM_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

/// An idle-only generation (learning after a turn), as opposed to an idle-only prewarm.
fn idle_generation(request: &Request) -> bool {
    request.idle_only && !matches!(request.work, Work::Prewarm)
}

struct Worker<B: Backend> {
    config: EngineConfig,
    engine: Arc<Engine<B>>,
    model: Arc<LanguageModel<B>>,
    compiler: GrammarCompiler,
    rx: mpsc::Receiver<Command>,
    slots: Vec<Slot<B>>,
    active: Vec<Active<B>>,
    waiting: VecDeque<Box<Request>>,
    hibernated: HashMap<String, Hibernated>,
    pending_hibernate: Vec<(String, HibernateReply)>,
    /// Conversations to drop once their requests have stopped ([`Command::Forget`]).
    pending_forget: Vec<String>,
    /// Conversations held resident while their turn runs, until when (a hold that is never
    /// released, its holder gone, lapses at `MAX_HOLD`).
    held: HashMap<String, Instant>,
    view: Arc<Mutex<View>>,
    device: Arc<Mutex<DeviceFactsJson>>,
    publisher: Publisher,
    costs: Arc<dyn CostSink>,
    /// Round-robin position per priority class.
    turn: [usize; 3],
    stop_tokens: Vec<u64>,
    shutting_down: bool,
    /// Hibernated conversations were handed to a successor (their files must outlive this process).
    exported: bool,
    /// The worker's own channel (the vision thread reports through it).
    commands: mpsc::Sender<Command>,
    vision: Option<VisionRuntime>,
    /// Images being encoded (cache keys).
    encoding: HashSet<String>,
    /// Image embeddings by `sha256:rowsxcolumns`, oldest first.
    embeddings: VecDeque<(String, Arc<[u16]>)>,
    /// The next request's number (`Request::id`).
    next_id: u64,
    /// Shared leading blocks, least recently used dropped first.
    seeds: Vec<Seed<B>>,
    /// When the engine last had other work than idle-only generations ([`IDLE_ROOM_AFTER`]).
    last_work: Instant,
    /// The idle-only request whose wait for room was last logged.
    idle_wait_logged: Option<u64>,
}

fn class(priority: Priority) -> usize {
    match priority {
        Priority::Foreground => 0,
        Priority::Agent => 1,
        Priority::Background => 2,
    }
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// File name for a key: its readable characters plus a hash, so distinct keys never collide.
fn file_stem(key: &str) -> String {
    let readable: String =
        key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(48).collect();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{readable}-{hash:016x}")
}

impl<B: Backend> Worker<B> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: EngineConfig,
        engine: Arc<Engine<B>>,
        model: Arc<LanguageModel<B>>,
        compiler: GrammarCompiler,
        rx: mpsc::Receiver<Command>,
        commands: mpsc::Sender<Command>,
        view: Arc<Mutex<View>>,
        device: Arc<Mutex<DeviceFactsJson>>,
        publisher: Publisher,
        costs: Arc<dyn CostSink>,
    ) -> Self {
        // Slots are made as conversations arrive (see `free_slot`): none are planned.
        let slots = Vec::new();
        let stop_tokens = model.generation_config().stop_token_ids.to_vec();
        let worker = Self {
            config,
            engine,
            model,
            compiler,
            rx,
            slots,
            active: Vec::new(),
            waiting: VecDeque::new(),
            hibernated: HashMap::new(),
            pending_hibernate: Vec::new(),
            pending_forget: Vec::new(),
            held: HashMap::new(),
            view,
            device,
            publisher,
            costs,
            turn: [0; 3],
            stop_tokens,
            shutting_down: false,
            exported: false,
            commands,
            vision: None,
            encoding: HashSet::new(),
            embeddings: VecDeque::new(),
            next_id: 1,
            seeds: Vec::new(),
            last_work: Instant::now(),
            idle_wait_logged: None,
        };
        worker.publish_view();
        worker
    }

    fn run(&mut self) {
        // Nothing waiting could be admitted on the last pass (with nothing running, only a waiting
        // request of a higher class can hold one back): block for the next command.
        let mut stalled = false;
        // A quiet deadline wakes admission once; a request that still cannot start then waits
        // for a state-changing command instead of polling the expired deadline.
        let mut idle_room_checked = None;
        loop {
            // Commands: block only when nothing is running and nothing waiting can start. A request
            // queued behind one that just finished or was cancelled (its conversation's slot was
            // busy) is admitted now, not when some later command happens to arrive.
            // Requests waiting for an image's embeddings do not run; with nothing runnable the
            // worker blocks for the next command (the vision thread's results are commands).
            let runnable = self.active.iter().any(Self::runnable);
            let idle = !runnable && (self.waiting.is_empty() || stalled);
            // The vision tower unloads once idle long enough; an idle worker wakes for it.
            let unload_at = self
                .vision
                .as_ref()
                .filter(|_| self.encoding.is_empty())
                .map(|vision| vision.last_used + self.config.vision_idle_unload);
            if unload_at.is_some_and(|at| Instant::now() >= at) {
                self.unload_vision();
            }
            self.spill_idle();
            // An idle worker also wakes when the next resident conversation is due to be spilled.
            // ... and when an idle-only generation's wait for the quiet period ends.
            let idle_room = self.waiting.iter().any(|request| idle_generation(request)).then(|| self.last_work + IDLE_ROOM_AFTER).filter(|at| Some(*at) != idle_room_checked);
            let wake = [unload_at.filter(|_| self.vision.is_some()), self.next_spill(), idle_room].into_iter().flatten().min();
            let command = if idle && !self.shutting_down {
                let received = match wake {
                    Some(at) => match self.rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(command) => Ok(Some(command)),
                        Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                        Err(mpsc::RecvTimeoutError::Disconnected) => Err(()),
                    },
                    None => self.rx.recv().map(Some).map_err(|_| ()),
                };
                match received {
                    Ok(command) => command,
                    Err(()) => {
                        self.shutting_down = true;
                        None
                    }
                }
            } else {
                match self.rx.try_recv() {
                    Ok(command) => Some(command),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.shutting_down = true;
                        None
                    }
                }
            };
            if let Some(command) = command {
                self.command(command);
                // Take every queued command before stepping, so a burst is admitted in order.
                while let Ok(command) = self.rx.try_recv() {
                    self.command(command);
                }
            }
            if self.shutting_down {
                self.stop_all();
                // Conversations parked on disk belong to this process unless a successor took them.
                if !self.exported {
                    for record in self.hibernated.values() {
                        let _ = std::fs::remove_file(&record.file);
                    }
                }
                break;
            }
            let admission_at = Instant::now();
            self.admit();
            let quiet_at = self.last_work + IDLE_ROOM_AFTER;
            if admission_at >= quiet_at {
                idle_room_checked = Some(quiet_at);
            }
            stalled = self.active.is_empty() && !self.waiting.is_empty();
            self.run_pending_hibernations();
            self.run_pending_forgets();
            if !self.active.is_empty() {
                self.step();
            }
        }
        // The vision thread first (it finishes its job), then streams (they borrow the model and
        // the states), then states, then the model.
        if let Some(vision) = self.vision.take() {
            vision.worker.stop();
        }
        self.active.clear();
        self.slots.clear();
    }

    /// Whether a request can take a step now: it is not waiting at an image being encoded.
    fn runnable(active: &Active<B>) -> bool {
        match active.phase {
            Phase::Generate(_) => true,
            Phase::Prefill { next } => !active.images.iter().any(|image| image.rows.is_none() && image.start <= next),
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Run(request) => {
                if request.prompt.is_empty() {
                    let _ = request.events.send(Event::Failed(Failure::Invalid("the prompt is empty".into())));
                } else if request.prompt.len() as u64 > u64::from(self.config.state_capacity) {
                    let _ = request.events.send(Event::Failed(Failure::Invalid(format!(
                        "the prompt has {} tokens; this load holds at most {} per conversation",
                        request.prompt.len(),
                        self.config.state_capacity
                    ))));
                } else {
                    let mut request = request;
                    request.id = self.next_id;
                    self.next_id += 1;
                    let ahead = self.waiting.iter().filter(|other| class(other.priority) <= class(request.priority)).count();
                    let mut event = crate::progress::event(request.id, &request.key, request.priority, matches!(request.work, Work::Prewarm), progress_phase::QUEUED, request.prompt.len());
                    event.ahead = ahead as u32;
                    self.publish_progress(&event);
                    self.waiting.push_back(request);
                }
            }
            Command::Hibernate { key, reply } => self.pending_hibernate.push((key, reply)),
            Command::Hold { key, hold } => {
                if hold {
                    self.held.insert(key, Instant::now() + MAX_HOLD);
                } else {
                    self.held.remove(&key);
                }
            }
            Command::Forget(key) => {
                for request in self.waiting.iter().filter(|request| request.key == key) {
                    request.stop.store(true, Ordering::SeqCst);
                }
                for active in self.active.iter().filter(|active| active.request.key == key) {
                    active.request.stop.store(true, Ordering::SeqCst);
                }
                self.held.remove(&key);
                self.pending_forget.push(key);
            }
            Command::HibernateAll { reply } => {
                let mut records = Vec::new();
                for index in 0..self.slots.len() {
                    if self.slots[index].busy.is_none()
                        && self.slots[index].key.is_some()
                        && let Ok(record) = self.hibernate_slot(index)
                    {
                        records.push(record);
                    }
                }
                records.extend(self.hibernated.values().cloned());
                records.sort_by(|a, b| a.key.cmp(&b.key));
                records.dedup_by(|a, b| a.key == b.key);
                self.exported = true;
                let _ = reply.send(records);
                self.publish_view();
            }
            Command::Adopt(records) => {
                for record in records {
                    if record.identity == self.config.identity && record.file.exists() {
                        self.hibernated.insert(record.key.clone(), record);
                    }
                }
                self.publish_view();
            }
            Command::Vision(message) => self.vision_message(message),
            Command::Shutdown => self.shutting_down = true,
        }
    }

    fn stop_all(&mut self) {
        for active in self.active.drain(..) {
            let _ = active.request.events.send(Event::Failed(Failure::Unavailable("the model is shutting down".into())));
        }
        for request in self.waiting.drain(..) {
            let _ = request.events.send(Event::Failed(Failure::Unavailable("the model is shutting down".into())));
        }
    }

    // ---- admission --------------------------------------------------------------------------

    fn resident_tokens(&self) -> u64 {
        self.slots.iter().map(|slot| u64::from(slot.n_tokens)).sum()
    }

    /// Pool tokens the resident conversations' fixed states stand for.
    fn state_tokens(&self) -> u64 {
        self.slots.iter().filter(|slot| slot.state.is_some()).count() as u64 * self.config.conversation_tokens
    }

    fn reserved_tokens(&self) -> u64 {
        self.active.iter().map(|active| active.reserve).sum()
    }

    /// Pool tokens the shared leading blocks stand for.
    fn seed_tokens(&self) -> u64 {
        self.seeds.iter().map(|seed| seed.tokens).sum()
    }

    /// Everything the pool holds besides `reserve`: conversations (their contexts and fixed
    /// states), running requests' reserves, the vision tower and the shared leading blocks.
    fn pool_used(&self) -> u64 {
        self.resident_tokens() + self.state_tokens() + self.reserved_tokens() + self.vision_tokens() + self.seed_tokens()
    }

    /// Drop the least recently used shared leading block (false when none is kept).
    fn drop_seed(&mut self) -> bool {
        let Some(position) = self.seeds.iter().enumerate().min_by_key(|(_, seed)| seed.last_used).map(|(position, _)| position) else {
            return false;
        };
        let seed = self.seeds.remove(position);
        tracing::info!(tokens = seed.snapshot.tokens().len(), "dropped a shared leading block");
        drop(seed);
        self.engine.trim_buffer_cache();
        self.refresh_device();
        true
    }

    /// Pool tokens the resident vision tower stands for (0 when it is not loaded).
    fn vision_tokens(&self) -> u64 {
        self.vision.as_ref().map_or(0, |vision| vision.resident_bytes.div_ceil(self.config.bytes_per_token.max(1)))
    }

    /// Requests in admission order: priority class, then arrival.
    fn waiting_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.waiting.len()).collect();
        order.sort_by_key(|&index| (class(self.waiting[index].priority), self.waiting[index].arrival));
        order
    }

    fn admit(&mut self) {
        // The last moment the engine had other work than idle-only generations.
        if self.active.iter().map(|active| &*active.request).chain(self.waiting.iter().map(|request| &**request)).any(|request| !idle_generation(request)) {
            self.last_work = Instant::now();
        }
        // Drop requests whose caller is gone before they start.
        let mut index = 0;
        while index < self.waiting.len() {
            let request = &self.waiting[index];
            // A prewarm waiting beside a real request for its conversation would only roll that
            // request's state back afterwards.
            let moot = matches!(request.work, Work::Prewarm)
                && crate::progress::prewarm_moot(
                    &request.key,
                    self.waiting
                        .iter()
                        .map(|other| (other.key.as_str(), matches!(other.work, Work::Prewarm)))
                        .chain(self.active.iter().map(|active| (active.request.key.as_str(), matches!(active.request.work, Work::Prewarm)))),
                );
            if moot || request.stop.load(Ordering::SeqCst) || request.events.is_closed() {
                if let Some(request) = self.waiting.remove(index) {
                    self.publish_ended(&request, None, "stopped");
                    let _ = request.events.send(Event::Failed(Failure::Cancelled));
                }
            } else {
                index += 1;
            }
        }
        loop {
            let mut admitted = false;
            for position in self.waiting_order() {
                match self.place(position) {
                    Placement::Admitted => {
                        admitted = true;
                        break;
                    }
                    Placement::Wait => {
                        // Strict order across classes: a request never overtakes a waiting
                        // request of a higher class; within a class, later requests may use a
                        // free slot while an earlier one waits for its busy key.
                        continue;
                    }
                }
            }
            if !admitted {
                break;
            }
        }
        self.publish_view();
    }

    /// Try to start the waiting request at `position`.
    fn place(&mut self, position: usize) -> Placement {
        // Work for one conversation stops the idle-only prewarms of the others, and an idle-only
        // prewarm waits while another conversation is served.
        let key = self.waiting[position].key.clone();
        // Quiet long enough, an idle-only generation is placed like any request (it still waits
        // while another conversation is served); before that, only in free room.
        if idle_generation(&self.waiting[position]) && self.last_work.elapsed() < IDLE_ROOM_AFTER {
            return self.place_idle(position, &key);
        }
        if self.waiting[position].idle_only {
            if self.serving_other(&key) {
                return Placement::Wait;
            }
        } else if !matches!(self.waiting[position].work, Work::Prewarm) {
            self.stop_idle_prewarms(&key);
        }
        // A prewarm of a leading block that is kept already is done without a slot.
        if let Some(seed) = self.kept_seed(&self.waiting[position]) {
            self.seeds[seed].last_used = Instant::now();
            let Some(request) = self.waiting.remove(position) else { return Placement::Wait };
            let cached = self.seeds[seed].snapshot.tokens().len() as u32;
            self.publish_ended(&request, None, "prefilled");
            let _ = request.events.send(Event::Done(Outcome {
                finish: Finish::Prefilled,
                prompt_tokens: request.prompt.len() as u32,
                cached_tokens: cached,
                completion_tokens: 0,
                queue_ms: ms(request.arrival),
                prefill_ms: 0.0,
                decode_ms: 0.0,
                draft_total: 0,
                draft_accepted: 0,
                slot: 0,
            }));
            return Placement::Admitted;
        }
        let priority = self.waiting[position].priority;
        // A higher class waiting for a slot blocks lower classes from taking one.
        let blocked_by_higher = self.waiting.iter().enumerate().any(|(index, other)| {
            index != position && class(other.priority) < class(priority) && !self.key_busy(&other.key)
        });
        let reserve = self.reserve_for(&self.waiting[position]);
        let slot = match self.slots.iter().position(|slot| slot.key.as_deref() == Some(key.as_str())) {
            Some(index) if self.slots[index].busy.is_some() => {
                self.preempt_prewarm(&key, priority, true);
                return Placement::Wait;
            }
            Some(index) => index,
            None => {
                if blocked_by_higher {
                    return Placement::Wait;
                }
                match self.free_slot(reserve) {
                    Some(index) => index,
                    None => {
                        self.preempt_prewarm(&key, priority, false);
                        return Placement::Wait;
                    }
                }
            }
        };
        // A slot without a state takes a conversation's fixed state from the pool when it starts.
        let need = reserve + if self.slots[slot].state.is_none() { self.config.conversation_tokens } else { 0 };
        // Room in the pool: spill idle conversations (least recently used first) to disk, then drop
        // shared leading blocks (a cache: the next conversation re-reads its block).
        while self.pool_used() + need > self.config.pool_tokens {
            let own = |index: usize, slot: &Slot<B>| index == slot_index_of_key(slot, &key, index);
            let candidate = self
                .slots
                .iter()
                .enumerate()
                .filter(|(index, slot)| slot.busy.is_none() && slot.n_tokens > 0 && !own(*index, slot))
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(index, _)| index);
            match candidate {
                Some(index) => {
                    if let Err(error) = self.hibernate_slot(index) {
                        tracing::warn!(%error, "spilling a conversation to disk failed; dropping it");
                        self.clear_slot(index);
                    }
                }
                None if self.drop_seed() => {}
                None => break,
            }
        }
        if self.pool_used() + need > self.config.pool_tokens
            && !self.active.is_empty()
        {
            // Running requests hold the pool; wait for one to finish.
            return Placement::Wait;
        }
        let Some(request) = self.waiting.remove(position) else { return Placement::Wait };
        self.start(slot, request, reserve);
        Placement::Admitted
    }

    /// Stop the running prewarm a waiting request of `priority` for `key` outranks (see
    /// [`crate::progress::prewarm_to_stop`]); it ends at the next step, its prefill kept.
    fn preempt_prewarm(&mut self, key: &str, priority: Priority, slot_free: bool) {
        let running: Vec<crate::progress::Running<'_>> = self
            .active
            .iter()
            .map(|active| crate::progress::Running {
                key: active.request.key.as_str(),
                priority: active.request.priority,
                prewarm: matches!(active.request.work, Work::Prewarm),
            })
            .collect();
        if let Some(index) = crate::progress::prewarm_to_stop(&running, key, priority, slot_free) {
            self.active[index].request.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Whether another conversation runs a generation or waits for non-idle work.
    /// Queued idle-only generations yield to each other in admission order instead of deadlocking.
    fn serving_other(&self, key: &str) -> bool {
        let real = |request: &Request| request.key != key && !matches!(request.work, Work::Prewarm);
        self.active.iter().any(|active| real(&active.request)) || self.waiting.iter().any(|request| real(request) && !idle_generation(request))
    }

    /// Stop the running idle-only prewarms of conversations other than `key` (at their next step;
    /// what they prefilled is kept).
    /// An idle-only generation (learning after a turn) takes nothing from anyone: it starts only
    /// while no other request runs or waits, on its own conversation's slot or an empty one, in the
    /// room the pool has (no conversation is spilled for it). Once running, the scheduling class
    /// puts every other request first.
    fn place_idle(&mut self, position: usize, key: &str) -> Placement {
        let others = self.active.iter().any(|active| active.request.key != key) || self.waiting.iter().enumerate().any(|(index, other)| index != position && !other.idle_only);
        let slot = self.slots.iter().position(|slot| slot.busy.is_none() && slot.key.as_deref() == Some(key)).or_else(|| self.slots.iter().position(|slot| slot.busy.is_none() && slot.key.is_none()));
        let reserve = self.reserve_for(&self.waiting[position]);
        let Some(slot) = slot else { return Placement::Wait };
        let need = reserve + if self.slots[slot].state.is_none() { self.config.conversation_tokens } else { 0 };
        if others || self.pool_used() + need > self.config.pool_tokens {
            if !others && self.idle_wait_logged != Some(self.waiting[position].id) {
                self.idle_wait_logged = Some(self.waiting[position].id);
                tracing::info!(pool_used = self.pool_used(), need, pool = self.config.pool_tokens, seeds = self.seed_tokens(), vision = self.vision_tokens(), "an idle-only request waits for room in the pool");
            }
            return Placement::Wait;
        }
        let Some(request) = self.waiting.remove(position) else { return Placement::Wait };
        self.start(slot, request, reserve);
        Placement::Admitted
    }

    fn stop_idle_prewarms(&mut self, key: &str) {
        for active in self.active.iter().filter(|active| active.request.idle_only && matches!(active.request.work, Work::Prewarm) && active.request.key != key) {
            active.request.stop.store(true, Ordering::SeqCst);
        }
    }

    fn key_busy(&self, key: &str) -> bool {
        self.slots.iter().any(|slot| slot.key.as_deref() == Some(key) && slot.busy.is_some())
    }

    /// A slot for a new conversation that adds `reserve` to the pool: an empty one; else a new one
    /// while the pool has room for another resident conversation (and the operator's cap, if any,
    /// allows it); else the least recently used idle one (its conversation is spilled to disk).
    /// None while every slot is busy and the pool is full.
    fn free_slot(&mut self, reserve: u64) -> Option<usize> {
        if let Some(index) = self.slots.iter().position(|slot| slot.busy.is_none() && slot.key.is_none()) {
            return Some(index);
        }
        let below_cap = self.config.max_resident.is_none_or(|cap| self.slots.len() < cap);
        let room = self.pool_used() + reserve + self.config.conversation_tokens <= self.config.pool_tokens;
        // Nothing idle to reuse: a new slot, and the pool makes room by spilling or waiting.
        let idle = self.slots.iter().any(|slot| slot.busy.is_none());
        if below_cap && (room || !idle) {
            self.slots.push(Slot {
                key: None,
                state: None,
                checkpoints: Vec::new(),
                n_tokens: 0,
                last_used: Instant::now(),
                busy: None,
                images: Vec::new(),
                pinned: None,
            });
            return Some(self.slots.len() - 1);
        }
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.busy.is_none())
            .min_by_key(|(_, slot)| slot.last_used)
            .map(|(index, _)| index)
    }

    /// Pool cells a request may add: its prompt beyond what its conversation already holds, plus its
    /// generation.
    fn reserve_for(&self, request: &Request) -> u64 {
        let generation = match &request.work {
            Work::Generate { max_tokens, .. } => max_tokens.map_or(GENERATION_RESERVE, u64::from),
            Work::Prewarm => 0,
        };
        let held = self
            .slots
            .iter()
            .find(|slot| slot.key.as_deref() == Some(request.key.as_str()))
            .map_or(0, |slot| u64::from(slot.n_tokens));
        (request.prompt.len() as u64).saturating_sub(held).saturating_add(generation).min(u64::from(self.config.state_capacity))
    }

    fn clear_slot(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.key = None;
        slot.state = None;
        slot.checkpoints.clear();
        slot.n_tokens = 0;
        slot.images.clear();
    }

    fn new_state(&self) -> Result<Box<LanguageModelState<B>>, String> {
        self.model
            .create_empty_state(Some(self.config.state_capacity), rand_seed())
            .map(Box::new)
            .map_err(|error| format!("cannot create a conversation state: {error}"))
    }

    /// Put `request` on `slot`: bring its conversation in (resident, restored from disk, or cold)
    /// and decide how much of the prompt is already processed.
    fn start(&mut self, slot_index: usize, request: Box<Request>, reserve: u64) {
        let queue_ms = ms(request.arrival);
        let key = request.key.clone();
        // A different conversation on this slot is spilled to disk first.
        if self.slots[slot_index].key.as_deref() != Some(key.as_str()) {
            if self.slots[slot_index].n_tokens > 0
                && let Err(error) = self.hibernate_slot(slot_index)
            {
                tracing::warn!(%error, "spilling a conversation to disk failed; dropping it");
            }
            self.clear_slot(slot_index);
            if let Some(record) = self.hibernated.remove(&key) {
                self.restore(slot_index, &record);
            }
        }
        let slot = &mut self.slots[slot_index];
        slot.key = Some(key);
        slot.last_used = Instant::now();
        slot.busy = Some(request.priority);
        let prompt = &request.prompt;
        let spans: Vec<ImageSpan> = request.images.iter().map(PromptImage::span).collect();
        let outcome = (|| -> Result<u32, String> {
            if self.slots[slot_index].state.is_none() {
                self.slots[slot_index].state = Some(self.new_state()?);
                self.slots[slot_index].checkpoints.clear();
                self.slots[slot_index].n_tokens = 0;
            }
            // At least the last prompt token is always fed: its forward pass samples the answer.
            // Compared by identity: an image's tokens match only the same image at the same grid.
            let state_tokens: Vec<u64> =
                self.slots[slot_index].state.as_ref().map(|state| state.tokens().to_vec()).unwrap_or_default();
            let state_identity = identity(&state_tokens, &self.slots[slot_index].images);
            let prompt_identity = identity(prompt, &spans);
            let common = state_identity.iter().zip(prompt_identity.iter()).take_while(|(a, b)| a == b).count();
            let usable = common.min(prompt.len() - 1);
            // Every checkpoint is a prefix of the state, so one within the usable part is a prefix of
            // the prompt: the longest such one.
            let best = self.slots[slot_index]
                .checkpoints
                .iter()
                .enumerate()
                .filter(|(_, checkpoint)| !checkpoint.snapshot.tokens().is_empty() && checkpoint.snapshot.tokens().len() <= usable)
                .max_by_key(|(_, checkpoint)| checkpoint.snapshot.tokens().len())
                .map(|(position, _)| position);
            let own = if usable == state_tokens.len() {
                usable
            } else {
                best.map_or(0, |position| self.slots[slot_index].checkpoints[position].snapshot.tokens().len())
            };
            // A shared leading block that covers more of the prompt than the conversation's own state
            // (a new conversation, or one whose history changed before it): start from a copy of it.
            // Compared by identity, so a prompt with an image inside the block never matches.
            let seed = self
                .seeds
                .iter()
                .enumerate()
                .filter(|(_, seed)| {
                    let tokens = seed.snapshot.tokens();
                    tokens.len() > own && tokens.len() < prompt.len() && prompt_identity[..tokens.len()] == tokens[..]
                })
                .max_by_key(|(_, seed)| seed.snapshot.tokens().len())
                .map(|(position, _)| position);
            if let Some(position) = seed {
                let mut state = self.new_state()?;
                self.model
                    .restore_state(&mut state, &self.seeds[position].snapshot)
                    .map_err(|error| format!("starting from a shared leading block failed: {error}"))?;
                let kept = self.seeds[position].snapshot.tokens().len();
                self.seeds[position].last_used = Instant::now();
                let slot = &mut self.slots[slot_index];
                slot.state = Some(state);
                // The previous state's checkpoints roll back a state that is gone.
                slot.checkpoints.clear();
                slot.n_tokens = kept as u32;
                tracing::debug!(key = %request.key, tokens = kept, "started from a shared leading block");
                return Ok(kept as u32);
            }
            if usable == state_tokens.len() {
                return Ok(usable as u32);
            }
            if let Some(position) = best {
                let slot = &mut self.slots[slot_index];
                if let Some(state) = slot.state.as_mut() {
                    self.model
                        .restore_state(state, &slot.checkpoints[position].snapshot)
                        .map_err(|error| format!("rolling back to a checkpoint failed: {error}"))?;
                    let kept = slot.checkpoints[position].snapshot.tokens().len();
                    slot.checkpoints[position].last_used = Instant::now();
                    // The rows after the rollback point will be overwritten: longer checkpoints are void.
                    slot.checkpoints.retain(|checkpoint| checkpoint.snapshot.tokens().len() <= kept);
                    slot.n_tokens = kept as u32;
                    return Ok(kept as u32);
                }
            }
            let fresh = self.new_state()?;
            let slot = &mut self.slots[slot_index];
            slot.state = Some(fresh);
            slot.checkpoints.clear();
            slot.n_tokens = 0;
            Ok(0)
        })();
        let cached = match outcome {
            Ok(cached) => cached,
            Err(error) => {
                self.slots[slot_index].busy = None;
                self.clear_slot(slot_index);
                let _ = request.events.send(Event::Failed(Failure::Engine(error)));
                return;
            }
        };
        // The images the prefill still feeds (any reaching past the reused prefix): their
        // embeddings, from the cache or encoded now.
        let mut images = Vec::new();
        for image in request.images.iter().filter(|image| image.start + image.tokens() > cached as usize) {
            match self.image_rows(image, request.vision.as_ref()) {
                Ok((key, rows)) => images.push(ActiveImage { start: image.start, grid: image.grid, key, rows }),
                Err(failure) => {
                    self.slots[slot_index].busy = None;
                    let _ = request.events.send(Event::Failed(failure));
                    self.publish_view();
                    return;
                }
            }
        }
        // A long read is where waits come from: say how much of the prompt was reused.
        let to_read = request.prompt.len().saturating_sub(cached as usize);
        if to_read >= LONG_PREFILL_TOKENS {
            tracing::info!(cache_key = %request.key, prompt_tokens = request.prompt.len(), reused_tokens = cached, to_read, prewarm = matches!(request.work, Work::Prewarm), "reading a long prompt");
        }
        // From here the state holds this prompt (as it is fed): its images describe it.
        self.slots[slot_index].images = spans;
        let _ = request.events.send(Event::Started {
            slot: slot_index as u32,
            queue_ms,
            cached_tokens: cached,
            prompt_tokens: request.prompt.len() as u32,
        });
        let thinking_budget = match &request.work {
            Work::Generate { think_budget: Some(_), .. } => Some(0),
            _ => None,
        };
        self.active.push(Active {
            slot: slot_index,
            request,
            phase: Phase::Prefill { next: cached as usize },
            cached,
            queue_ms,
            admitted: Instant::now(),
            reserve,
            first_token: None,
            last_token: None,
            generated: 0,
            draft: (0, 0),
            emitted: Vec::new(),
            thinking: thinking_budget,
            images,
            reporter: crate::progress::Reporter::default(),
        });
        let index = self.active.len() - 1;
        self.report(index, progress_phase::PREFILL);
    }

    // ---- stepping -----------------------------------------------------------------------------

    /// Advance one active request by one step.
    fn step(&mut self) {
        // Stopped requests end first (cancellation is prompt).
        if let Some(index) = self.active.iter().position(|active| {
            active.request.stop.load(Ordering::SeqCst) || active.request.events.is_closed()
        }) {
            self.finish(index, Finish::Stopped);
            return;
        }
        let Some(best) =
            self.active.iter().filter(|active| Self::runnable(active)).map(|active| class(active.request.priority)).min()
        else {
            return;
        };
        let candidates: Vec<usize> = (0..self.active.len())
            .filter(|&index| Self::runnable(&self.active[index]) && class(self.active[index].request.priority) == best)
            .collect();
        let turn = &mut self.turn[best];
        *turn = (*turn + 1) % candidates.len().max(1);
        let index = candidates[*turn % candidates.len()];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.advance(index)));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.fail(index, error),
            Err(panic) => {
                let message = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                    .unwrap_or_else(|| "engine panic".to_owned());
                tracing::error!(%message, "the engine panicked; the conversation is dropped");
                self.fail_hard(index, format!("engine error: {message}"));
            }
        }
    }

    fn advance(&mut self, index: usize) -> Result<(), String> {
        let slot_index = self.active[index].slot;
        let prefill_from = match &self.active[index].phase {
            Phase::Prefill { next } => Some(*next),
            Phase::Generate(_) => None,
        };
        match prefill_from {
            Some(start) => {
                let request = &self.active[index].request;
                let end = match (request.shared_only, request.shared) {
                    (true, Some(shared)) => shared,
                    _ => request.prompt.len() - 1,
                };
                if self.kept_seed(request).is_some() {
                    // Another conversation read the block meanwhile: it is kept already.
                    self.finish(index, Finish::Prefilled);
                    return Ok(());
                }
                if start < end {
                    // Up to the first image still being encoded (the request waits there), and to the
                    // next reuse point (checkpointed when reached).
                    let pending = self.active[index].images.iter().filter(|image| image.rows.is_none()).map(|image| image.start).min();
                    let reuse = self.active[index].request.checkpoints.iter().copied().filter(|&point| point > start && point < end).min();
                    let stop = (start + self.config.prefill_chunk).min(end).min(pending.unwrap_or(usize::MAX)).min(reuse.unwrap_or(usize::MAX));
                    if stop <= start {
                        return Ok(());
                    }
                    let chunk = self.active[index].request.prompt[start..stop].to_vec();
                    let mut options = self.model.default_stream_options();
                    options.sampling_method = SamplingMethod::Greedy;
                    // Images overlapping this chunk, placed relative to it.
                    let images: Vec<ImageEmbedding> = self.active[index]
                        .images
                        .iter()
                        .filter(|image| image.start < stop && image.end() > start)
                        .map(|image| {
                            image.rows.clone().map(|rows| ImageEmbedding { start: image.start as i64 - start as i64, grid: image.grid, rows })
                        })
                        .collect::<Option<Vec<_>>>()
                        .ok_or("an image's embeddings are missing")?;
                    if !images.is_empty() {
                        let section = self.active[index]
                            .request
                            .vision
                            .as_ref()
                            .map(|vision| vision.mrope_section)
                            .ok_or("an image request without a vision configuration")?;
                        options.images = Some(ImageInput { images, mrope_section: section });
                    }
                    let state = self.slots[slot_index].state.as_mut().ok_or("the conversation state is missing")?;
                    // The stream prefills the chunk (and samples one token, discarded) when created;
                    // dropping it waits for the device.
                    let stream = self.model.stream(&chunk, state, options).map_err(|error| format!("prefill failed: {error}"))?;
                    drop(stream);
                    state.clear_pending_output();
                    self.slots[slot_index].n_tokens = stop as u32;
                    if let Phase::Prefill { next } = &mut self.active[index].phase {
                        *next = stop;
                    }
                    if reuse == Some(stop) {
                        self.checkpoint(slot_index, stop);
                        self.seed(index, stop);
                    }
                    self.report(index, progress_phase::PREFILL);
                    return Ok(());
                }
                // Everything but the last token is in the state: checkpoint it, then generate.
                self.active[index].images.clear();
                self.seed(index, end);
                if self.active[index].request.shared_only {
                    self.finish(index, Finish::Prefilled);
                    return Ok(());
                }
                self.checkpoint(slot_index, end);
                let last = self.active[index].request.prompt[end];
                if matches!(self.active[index].request.work, Work::Prewarm) {
                    self.finish(index, Finish::Prefilled);
                    return Ok(());
                }
                let generation = self.open_stream(index, &[last])?;
                self.active[index].phase = Phase::Generate(Box::new(generation));
                Ok(())
            }
            None => {
                let Phase::Generate(generation) = &mut self.active[index].phase else { return Ok(()) };
                let next = generation.stream.next();
                let max_tokens = generation.max_tokens;
                let stop_tokens = generation.stop_tokens.clone();
                match next {
                    None => self.finish(index, Finish::Length),
                    Some(Err(error)) => return Err(format!("generation failed: {error}")),
                    Some(Ok(token)) => {
                        let now = Instant::now();
                        let active = &mut self.active[index];
                        active.first_token.get_or_insert(now);
                        active.last_token = Some(now);
                        active.generated += 1;
                        if stop_tokens.contains(&token) {
                            self.finish(index, Finish::Stop);
                            return Ok(());
                        }
                        let _ = active.request.events.send(Event::Token(token as u32));
                        active.emitted.push(token);
                        self.report(index, progress_phase::DECODE);
                        let active = &mut self.active[index];
                        if let (Some(used), Work::Generate { think_budget: Some(budget), .. }) = (active.thinking, &active.request.work) {
                            if active.emitted.ends_with(&budget.end) {
                                active.thinking = None;
                            } else if used + 1 >= budget.tokens {
                                return self.close_thinking(index);
                            } else {
                                active.thinking = Some(used + 1);
                            }
                        }
                        let active = &mut self.active[index];
                        if max_tokens.is_some_and(|max| active.generated >= max) {
                            self.finish(index, Finish::Length);
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// Keep a checkpoint of the slot's state at `length` tokens (when the state holds exactly that
    /// many and none is kept there yet), evicting the least recently used beyond the fitted count.
    fn checkpoint(&mut self, slot_index: usize, length: usize) {
        let slot = &self.slots[slot_index];
        if length == 0 || slot.checkpoints.iter().any(|checkpoint| checkpoint.snapshot.tokens().len() == length) {
            return;
        }
        let Some(state) = slot.state.as_ref().filter(|state| state.tokens().len() == length) else { return };
        match self.model.snapshot_state(state, SnapshotMode::Checkpoint) {
            Ok(snapshot) => {
                let pinned = self.slots[slot_index].pinned;
                let checkpoints = &mut self.slots[slot_index].checkpoints;
                checkpoints.push(Checkpoint { snapshot, last_used: Instant::now() });
                // The leading block's checkpoint is kept on top of the fitted count: counted inside
                // it, it pushed out the checkpoint of the conversation so far on every call (seen:
                // every step re-read everything after the system block).
                while checkpoints.iter().filter(|checkpoint| Some(checkpoint.snapshot.tokens().len()) != pinned).count() > self.config.checkpoints.max(1) {
                    // The leading block's checkpoint stays; the least recently used other one goes.
                    let Some(oldest) = checkpoints
                        .iter()
                        .enumerate()
                        .filter(|(_, checkpoint)| Some(checkpoint.snapshot.tokens().len()) != pinned)
                        .min_by_key(|(_, checkpoint)| checkpoint.last_used)
                        .map(|(position, _)| position)
                    else {
                        break;
                    };
                    checkpoints.remove(oldest);
                }
            }
            Err(error) => tracing::warn!(%error, "taking a context checkpoint failed"),
        }
    }

    /// The seed that already holds a `shared_only` request's leading block.
    fn kept_seed(&self, request: &Request) -> Option<usize> {
        let shared = request.shared.filter(|_| request.shared_only)?;
        self.seeds.iter().position(|seed| seed.snapshot.tokens() == &request.prompt[..shared])
    }

    /// Keep a copy of active request `index`'s leading system block when its slot's state holds
    /// exactly that (`length` is its `shared` point) and no copy of the same tokens is kept. Older
    /// copies make room for it; it is not kept when the pool has no room even then.
    fn seed(&mut self, index: usize, length: usize) {
        let request = &self.active[index].request;
        if request.shared != Some(length) || request.images.iter().any(|image| image.start < length) {
            return;
        }
        let pin_slot = self.active[index].slot;
        self.slots[pin_slot].pinned = Some(length);
        let block = &request.prompt[..length];
        if let Some(seed) = self.seeds.iter_mut().find(|seed| seed.snapshot.tokens() == block) {
            seed.last_used = Instant::now();
            return;
        }
        let slot_index = self.active[index].slot;
        let Some(state) = self.slots[slot_index].state.as_ref().filter(|state| state.tokens().len() == length) else { return };
        let snapshot = match self.model.snapshot_state(state, SnapshotMode::Full) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "copying a leading block failed");
                return;
            }
        };
        let tokens = (snapshot.buffer_bytes() as u64).div_ceil(self.config.bytes_per_token.max(1));
        while self.seeds.len() >= SEEDS || (self.pool_used() + tokens > self.config.pool_tokens && !self.seeds.is_empty()) {
            self.drop_seed();
        }
        if self.pool_used() + tokens > self.config.pool_tokens {
            tracing::debug!(length, tokens, "no pool room for a shared leading block");
            return;
        }
        tracing::info!(length, tokens, bytes = snapshot.buffer_bytes(), "keeping a shared leading block");
        self.seeds.push(Seed { snapshot, tokens, last_used: Instant::now() });
        self.refresh_device();
    }

    fn open_stream(&mut self, index: usize, input: &[u64]) -> Result<Generation<B>, String> {
        let slot_index = self.active[index].slot;
        let Work::Generate { max_tokens, sampling, grammar, grammar_trigger, .. } = &self.active[index].request.work else {
            return Err("not a generation".into());
        };
        // After a forced end of reasoning the grammar applies from the first generated token.
        let grammar_trigger = if self.active[index].emitted.is_empty() { grammar_trigger.clone() } else { None };
        let mut options: LanguageModelStreamOptions = self.model.default_stream_options();
        options.sampling_method = sampling.method.clone();
        options.speculation = self.config.speculation && sampling.allows_speculation;
        options.penalty_window_includes_state = sampling.penalty_window_includes_state;
        if let Some(config) = grammar {
            let grammar = Grammar::with_compiler(config, &self.compiler, grammar_trigger)
                .map_err(|error| format!("the grammar does not compile: {error}"))?;
            options.grammar = Some(grammar);
        }
        let max_tokens = *max_tokens;
        let seed = sampling.seed;
        let state = self.slots[slot_index].state.as_mut().ok_or("the conversation state is missing")?;
        state.set_sampling_seed(seed);
        let state_ptr: *mut LanguageModelState<B> = &mut **state;
        let model_ptr: *const LanguageModel<B> = Arc::as_ptr(&self.model);
        // SAFETY: see `Generation`: the model Arc is held by the worker past every stream, and the
        // slot's boxed state stays in place and is not otherwise accessed while the slot is busy.
        let (model, state): (&'static LanguageModel<B>, &'static mut LanguageModelState<B>) =
            unsafe { (&*model_ptr, &mut *state_ptr) };
        let stream = model.stream(input, state, options).map_err(|error| format!("generation failed: {error}"))?;
        Ok(Generation { stream, max_tokens, stop_tokens: self.stop_tokens.clone() })
    }

    /// End the request's stream (if any), adding its draft counts to the request's.
    fn end_stream(active: &mut Active<B>) {
        if let Phase::Generate(generation) = std::mem::replace(&mut active.phase, Phase::Prefill { next: 0 }) {
            let metrics = generation.stream.metrics().clone();
            let passes = metrics.num_decode_forward_passes + metrics.num_prefill_forward_passes;
            active.draft.0 += metrics.num_tokens_proposed.saturating_sub(passes) as u32;
            active.draft.1 += metrics.num_tokens_accepted.saturating_sub(passes) as u32;
            // Dropping the stream waits for the device and folds accepted draft tokens in; a sampled
            // token not yet handed out is left pending and is discarded by the next stream's input.
            drop(generation);
        }
    }

    /// The reasoning budget is reached: feed the closing tokens (handed out like generated ones, so
    /// the output parser sees reasoning end) and go on generating the answer on the same state.
    fn close_thinking(&mut self, index: usize) -> Result<(), String> {
        let Work::Generate { think_budget: Some(budget), .. } = &self.active[index].request.work else {
            return Ok(());
        };
        let close = budget.close.clone();
        let used = self.active[index].thinking.unwrap_or(0) + 1;
        tracing::info!(reasoning_tokens = used, "reasoning budget reached; ending the reasoning block");
        Self::end_stream(&mut self.active[index]);
        let slot_index = self.active[index].slot;
        let active = &mut self.active[index];
        active.thinking = None;
        for token in &close {
            let _ = active.request.events.send(Event::Token(*token as u32));
        }
        let state = self.slots[slot_index].state.as_ref().ok_or("the conversation state is missing")?;
        let held = state.tokens().len();
        let mut full = active.request.prompt.clone();
        full.extend_from_slice(&active.emitted);
        if held > full.len() || state.tokens() != &full[..held] {
            return Err("the conversation state does not match the generated text after the reasoning budget".into());
        }
        active.emitted.extend_from_slice(&close);
        active.generated += close.len() as u32;
        let mut input = full[held..].to_vec();
        input.extend_from_slice(&close);
        let generation = self.open_stream(index, &input)?;
        self.active[index].phase = Phase::Generate(Box::new(generation));
        Ok(())
    }

    fn release(&mut self, index: usize) -> Active<B> {
        let mut active = self.active.remove(index);
        Self::end_stream(&mut active);
        let slot = &mut self.slots[active.slot];
        slot.busy = None;
        slot.last_used = Instant::now();
        if let Some(state) = slot.state.as_ref() {
            slot.n_tokens = state.tokens().len() as u32;
        }
        active
    }

    fn finish(&mut self, index: usize, finish: Finish) {
        let active = self.release(index);
        // An idle-only generation leaves nothing in the pool, like a leading block's prewarm.
        if active.request.shared_only || (active.request.idle_only && !matches!(active.request.work, Work::Prewarm)) {
            // The block lives on as a seed (when it was reached); no conversation has this key.
            self.clear_slot(active.slot);
            self.engine.trim_buffer_cache();
        }
        let (draft_total, draft_accepted) = active.draft;
        let prefill_ms = active.first_token.map_or(ms(active.admitted), |first| {
            first.duration_since(active.admitted).as_secs_f64() * 1000.0
        });
        let decode_ms = match (active.first_token, active.last_token) {
            (Some(first), Some(last)) => last.duration_since(first).as_secs_f64() * 1000.0,
            _ => 0.0,
        };
        let outcome = Outcome {
            finish,
            prompt_tokens: active.request.prompt.len() as u32,
            cached_tokens: active.cached,
            completion_tokens: active.generated,
            queue_ms: active.queue_ms,
            prefill_ms,
            decode_ms,
            draft_total,
            draft_accepted,
            slot: active.slot as u32,
        };
        let finished = match finish {
            Finish::Stop => "stop",
            Finish::Length => "length",
            Finish::Stopped => "stopped",
            Finish::Prefilled => "prefilled",
        };
        self.publish_ended(&active.request, Some(&active), finished);
        let _ = active.request.events.send(Event::Done(outcome));
        self.refresh_device();
        self.publish_view();
    }

    fn fail(&mut self, index: usize, error: String) {
        self.fail_with(index, Failure::Engine(error));
    }

    fn fail_with(&mut self, index: usize, failure: Failure) {
        let active = self.release(index);
        // The state may be inconsistent after an engine error: start that conversation cold.
        self.clear_slot(active.slot);
        self.publish_ended(&active.request, Some(&active), "failed");
        let _ = active.request.events.send(Event::Failed(failure));
        self.publish_view();
    }

    /// After a panic inside uzu the stream may not be droppable safely; it is leaked.
    fn fail_hard(&mut self, index: usize, error: String) {
        let mut active = self.active.remove(index);
        if let Phase::Generate(generation) = std::mem::replace(&mut active.phase, Phase::Prefill { next: 0 }) {
            std::mem::forget(generation);
        }
        let slot = &mut self.slots[active.slot];
        slot.busy = None;
        if let Some(state) = slot.state.take() {
            std::mem::forget(state);
        }
        self.clear_slot(active.slot);
        self.publish_ended(&active.request, Some(&active), "failed");
        let _ = active.request.events.send(Event::Failed(Failure::Engine(error)));
        self.publish_view();
    }

    // ---- hibernation --------------------------------------------------------------------------

    /// Whether slot `index` holds a conversation nothing is running or waiting for.
    fn idle_conversation(&self, index: usize) -> bool {
        let slot = &self.slots[index];
        match &slot.key {
            Some(key) => slot.busy.is_none() && !self.waiting.iter().any(|request| &request.key == key),
            None => false,
        }
    }

    /// When the longest-idle resident conversation is due to be spilled.
    fn next_spill(&self) -> Option<Instant> {
        if self.config.idle_spill.is_zero() {
            return None;
        }
        (0..self.slots.len())
            .filter(|&index| self.idle_conversation(index))
            .map(|index| self.spill_due(index))
            .min()
    }

    /// When an idle resident conversation is due to be spilled: `idle_spill` after its last use,
    /// or, while its turn holds it, when the hold lapses.
    fn spill_due(&self, index: usize) -> Instant {
        let idle = self.slots[index].last_used + self.config.idle_spill;
        match self.slots[index].key.as_ref().and_then(|key| self.held.get(key)) {
            Some(&until) => until.max(idle),
            None => idle,
        }
    }

    /// Spill every conversation idle for `idle_spill` to disk (an empty one is just freed).
    fn spill_idle(&mut self) {
        if self.config.idle_spill.is_zero() {
            return;
        }
        let now = Instant::now();
        let due: Vec<usize> = (0..self.slots.len())
            .filter(|&index| self.idle_conversation(index) && self.spill_due(index) <= now)
            .collect();
        if due.is_empty() {
            return;
        }
        for index in due {
            if self.slots[index].n_tokens == 0 {
                self.clear_slot(index);
                continue;
            }
            match self.hibernate_slot(index) {
                Ok(record) => tracing::info!(cache_key = %record.key, tokens = record.n_tokens, bytes = record.bytes, "spilled an idle conversation to disk"),
                Err(error) => {
                    tracing::warn!(%error, "spilling an idle conversation to disk failed; dropping it");
                    self.clear_slot(index);
                }
            }
        }
        self.publish_view();
    }

    /// Delete the oldest spill files while they take more than `hibernate_limit` bytes.
    fn bound_spill_files(&mut self) {
        if self.config.hibernate_limit == 0 {
            return;
        }
        let mut total: u64 = self.hibernated.values().map(|record| record.bytes).sum();
        while total > self.config.hibernate_limit {
            let modified = |record: &Hibernated| std::fs::metadata(&record.file).and_then(|meta| meta.modified()).ok();
            let Some(oldest) = self.hibernated.values().min_by_key(|record| modified(record)).map(|record| record.key.clone()) else {
                break;
            };
            let Some(record) = self.hibernated.remove(&oldest) else { break };
            let _ = std::fs::remove_file(&record.file);
            total = total.saturating_sub(record.bytes);
            tracing::info!(cache_key = %record.key, bytes = record.bytes, "deleted the oldest spilled conversation (disk bound)");
        }
    }

    /// Drop the conversations asked for whose requests have stopped.
    fn run_pending_forgets(&mut self) {
        let pending = std::mem::take(&mut self.pending_forget);
        for key in pending {
            if self.slots.iter().any(|slot| slot.key.as_deref() == Some(key.as_str()) && slot.busy.is_some()) {
                self.pending_forget.push(key);
                continue;
            }
            if let Some(index) = self.slots.iter().position(|slot| slot.key.as_deref() == Some(key.as_str())) {
                self.clear_slot(index);
            }
            if let Some(record) = self.hibernated.remove(&key) {
                let _ = std::fs::remove_file(&record.file);
            }
            tracing::info!(cache_key = %key, "dropped a deleted conversation");
            self.publish_view();
        }
    }

    fn run_pending_hibernations(&mut self) {
        let pending = std::mem::take(&mut self.pending_hibernate);
        for (key, reply) in pending {
            match self.slots.iter().position(|slot| slot.key.as_deref() == Some(key.as_str())) {
                Some(index) if self.slots[index].busy.is_some() => self.pending_hibernate.push((key, reply)),
                Some(index) => {
                    let started = Instant::now();
                    let result = self.hibernate_slot(index).map(|record| {
                        json!({
                            "cache_key": record.key,
                            "slot": index,
                            "file": record.file,
                            "n_tokens": record.n_tokens,
                            "bytes": record.bytes,
                            "save_ms": ms(started),
                        })
                    });
                    let _ = reply.send(result.map_err(|error| ("internal".to_owned(), error)));
                }
                None => match self.hibernated.get(&key) {
                    Some(record) => {
                        let _ = reply.send(Ok(json!({
                            "cache_key": key,
                            "already": true,
                            "file": record.file,
                            "n_tokens": record.n_tokens,
                            "bytes": record.bytes,
                        })));
                    }
                    None => {
                        let _ = reply.send(Err(("not_found".to_owned(), format!("no conversation holds cache key `{key}`"))));
                    }
                },
            }
        }
        self.publish_view();
    }

    /// Write slot `index`'s conversation to disk and free the slot.
    fn hibernate_slot(&mut self, index: usize) -> Result<Hibernated, String> {
        let key = self.slots[index].key.clone().ok_or("the slot holds no conversation")?;
        let started = Instant::now();
        let state = self.slots[index].state.as_ref().ok_or("the slot holds no state")?;
        let snapshot =
            self.model.snapshot_state(state, SnapshotMode::Full).map_err(|error| format!("snapshot failed: {error}"))?;
        std::fs::create_dir_all(&self.config.hibernate_dir)
            .map_err(|error| format!("cannot create {}: {error}", self.config.hibernate_dir.display()))?;
        let file = self.config.hibernate_dir.join(format!("{}-{}.state", file_stem(&key), cc_protocol::contracts::now_ms()));
        let n_tokens = snapshot.tokens().len() as u32;
        // The context checkpoints travel along (their recurrent states; the attention rows they
        // need are in the full snapshot), so the conversation's reuse points survive the restore.
        let checkpoints = &self.slots[index].checkpoints;
        let header = serde_json::to_vec(&json!({"identity": self.config.identity, "cache_key": key, "n_tokens": n_tokens, "checkpoints": checkpoints.len(), "images": self.slots[index].images}))
            .map_err(|error| error.to_string())?;
        let written = (|| -> Result<u64, String> {
            let out = std::fs::File::create(&file).map_err(|error| format!("cannot write {}: {error}", file.display()))?;
            let mut out = BufWriter::with_capacity(8 << 20, out);
            out.write_all(HIBERNATION_MAGIC).map_err(|error| error.to_string())?;
            out.write_all(&(header.len() as u32).to_le_bytes()).map_err(|error| error.to_string())?;
            out.write_all(&header).map_err(|error| error.to_string())?;
            let mut body = snapshot.write_to(&mut out).map_err(|error| format!("cannot write the state: {error}"))?;
            for checkpoint in checkpoints {
                body += checkpoint.snapshot.write_to(&mut out).map_err(|error| format!("cannot write a checkpoint: {error}"))?;
            }
            out.flush().map_err(|error| error.to_string())?;
            Ok(12 + header.len() as u64 + body)
        })();
        let bytes = match written {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = std::fs::remove_file(&file);
                return Err(error);
            }
        };
        let save_ms = ms(started);
        self.costs.saved(u64::from(n_tokens), bytes, save_ms);
        drop(snapshot);
        self.clear_slot(index);
        // The snapshot's copies (and the freed state's dense buffers) leave uzu's buffer cache.
        self.engine.trim_buffer_cache();
        self.refresh_device();
        let record = Hibernated { key: key.clone(), file, n_tokens, bytes, identity: self.config.identity.clone() };
        self.hibernated.insert(key, record.clone());
        self.bound_spill_files();
        Ok(record)
    }

    /// Bring a hibernated conversation back into slot `index` (a failed restore leaves it cold).
    fn restore(&mut self, index: usize, record: &Hibernated) {
        let started = Instant::now();
        let outcome = (|| -> Result<Restored<B>, String> {
            let file = std::fs::File::open(&record.file).map_err(|error| format!("cannot open {}: {error}", record.file.display()))?;
            let mut input = BufReader::with_capacity(8 << 20, file);
            let mut magic = [0u8; 8];
            input.read_exact(&mut magic).map_err(|error| error.to_string())?;
            if &magic != HIBERNATION_MAGIC {
                return Err("not a hibernated conversation".into());
            }
            let mut length = [0u8; 4];
            input.read_exact(&mut length).map_err(|error| error.to_string())?;
            let mut header = vec![0u8; u32::from_le_bytes(length) as usize];
            input.read_exact(&mut header).map_err(|error| error.to_string())?;
            let header: Value = serde_json::from_slice(&header).map_err(|error| error.to_string())?;
            if header["identity"] != self.config.identity.as_str() {
                return Err("the state belongs to other weights".into());
            }
            let (snapshot, mut read) =
                self.model.read_state_snapshot(&mut input).map_err(|error| format!("cannot read the state: {error}"))?;
            // A file of an earlier version records one checkpoint as `"checkpoint": true`.
            let count = header["checkpoints"].as_u64().unwrap_or(u64::from(header["checkpoint"] == true));
            let mut checkpoints = Vec::new();
            for _ in 0..count {
                let (snapshot, bytes) =
                    self.model.read_state_snapshot(&mut input).map_err(|error| format!("cannot read a checkpoint: {error}"))?;
                read += bytes;
                checkpoints.push(Checkpoint { snapshot, last_used: Instant::now() });
            }
            // Beyond this load's count, the longest are kept (the latest reuse points).
            checkpoints.sort_by_key(|checkpoint| std::cmp::Reverse(checkpoint.snapshot.tokens().len()));
            checkpoints.truncate(self.config.checkpoints.max(1));
            let mut state = self.new_state()?;
            self.model.restore_state(&mut state, &snapshot).map_err(|error| format!("cannot restore the state: {error}"))?;
            let images: Vec<ImageSpan> = serde_json::from_value(header["images"].clone()).unwrap_or_default();
            Ok((state, checkpoints, images, read + 12 + header.to_string().len() as u64))
        })();
        let restore_ms = ms(started);
        let _ = std::fs::remove_file(&record.file);
        let event = match outcome {
            Ok((state, checkpoints, images, bytes)) => {
                let n_restored = state.tokens().len() as u32;
                let slot = &mut self.slots[index];
                slot.state = Some(state);
                slot.checkpoints = checkpoints;
                slot.images = images;
                slot.n_tokens = n_restored;
                slot.key = Some(record.key.clone());
                self.costs.restored(u64::from(n_restored), bytes, restore_ms);
                tracing::info!(cache_key = %record.key, tokens = n_restored, bytes, restore_ms, "restored a spilled conversation from disk");
                self.engine.trim_buffer_cache();
                self.refresh_device();
                json!({"cache_key": record.key, "slot": index, "ok": true, "n_restored": n_restored, "bytes": bytes, "restore_ms": restore_ms})
            }
            Err(error) => {
                tracing::warn!(%error, key = %record.key, "restoring a hibernated conversation failed; it runs cold");
                json!({"cache_key": record.key, "slot": index, "ok": false, "error": error, "restore_ms": restore_ms})
            }
        };
        (self.publisher)("model.restore", event);
    }

    // ---- images -------------------------------------------------------------------------------

    /// An image's cache key and embeddings: from the cache, or None after its encoding was handed
    /// to the vision thread (started, with the tower, when needed and when memory allows).
    fn image_rows(&mut self, image: &PromptImage, vision: Option<&Arc<VisionConfig>>) -> Result<ImageRows, Failure> {
        let vision = vision.ok_or_else(|| Failure::Invalid("image input is not available for this model".into()))?;
        let key = format!("{}:{}x{}", image.sha256, image.grid.0, image.grid.1);
        if let Some(position) = self.embeddings.iter().position(|(cached, _)| *cached == key) {
            let entry = self.embeddings.remove(position).ok_or_else(|| Failure::Engine("embedding cache".into()))?;
            let rows = entry.1.clone();
            self.embeddings.push_back(entry);
            return Ok((key, Some(rows)));
        }
        if self.encoding.contains(&key) {
            return Ok((key, None));
        }
        let patches = u64::from(image.grid.0) * u64::from(image.grid.1) * u64::from(vision.spatial_merge_size).pow(2);
        let engine_config = vision.engine();
        let mut need = engine_config.scratch_bytes(patches);
        if self.vision.is_none() {
            need += engine_config.resident_bytes();
        }
        self.make_room(need)?;
        if self.vision.is_none() {
            self.start_vision(vision)?;
        }
        let job = vision_worker::Job {
            key: key.clone(),
            path: image.path.clone(),
            sha256: image.sha256.clone(),
            grid: image.grid,
            max_pixels: self.config.vision_max_pixels,
        };
        let runtime = self.vision.as_mut().ok_or_else(|| Failure::Engine("the vision thread is not running".into()))?;
        runtime.worker.submit(job).map_err(Failure::Engine)?;
        runtime.last_used = Instant::now();
        self.encoding.insert(key.clone());
        Ok((key, None))
    }

    /// Make `need` bytes of headroom on the device: the allocation now plus `need` must stay
    /// within the memory budget. Idle conversations are hibernated, least recently used first,
    /// until it does; when nothing idle is left the image is refused (never an out-of-memory).
    fn make_room(&mut self, need: u64) -> Result<(), Failure> {
        let budget = self.config.memory_budget.load(Ordering::SeqCst);
        if budget == 0 {
            return Ok(());
        }
        let first = self.engine.device_facts().allocated_bytes;
        let mut hibernated = 0u32;
        loop {
            let Some(allocated) = self.engine.device_facts().allocated_bytes else { return Ok(()) };
            if allocated.saturating_add(need) <= budget {
                if hibernated > 0 {
                    let freed = first.unwrap_or(allocated).saturating_sub(allocated);
                    tracing::info!(hibernated, freed, need, "idle conversations hibernated to make room for vision");
                    (self.publisher)("model.vision", json!({"event": "room", "hibernated": hibernated, "freed_bytes": freed, "need_bytes": need, "allocated_bytes": allocated, "budget_bytes": budget}));
                }
                return Ok(());
            }
            let candidate = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.busy.is_none() && slot.state.is_some())
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(index, _)| index);
            match candidate {
                Some(index) => {
                    if let Err(error) = self.hibernate_slot(index) {
                        tracing::warn!(%error, "hibernating a conversation for vision failed; dropping it");
                        self.clear_slot(index);
                        self.engine.trim_buffer_cache();
                    }
                    hibernated += 1;
                }
                None if self.drop_seed() => {}
                None => {
                    let mib = |bytes: u64| format!("{:.1}", bytes as f64 / f64::from(1u32 << 20));
                    return Err(Failure::Unavailable(format!(
                        "not enough memory to load vision now: it needs {} MiB, {} MiB of the {} MiB budget are in use{}",
                        mib(need),
                        mib(allocated),
                        mib(budget),
                        if hibernated > 0 { format!(" after hibernating {hibernated} idle conversations") } else { String::new() }
                    )));
                }
            }
        }
    }

    /// Start the vision thread (it loads the tower and reports `Loaded`).
    fn start_vision(&mut self, vision: &Arc<VisionConfig>) -> Result<(), Failure> {
        let dir = self.config.vision_dir.clone().ok_or_else(|| Failure::Invalid("image input is not enabled for this model".into()))?;
        if vision.out_hidden_size != self.model.model_dim() {
            return Err(Failure::Invalid(format!(
                "the vision tower produces {}-wide embeddings; this model's width is {}",
                vision.out_hidden_size,
                self.model.model_dim()
            )));
        }
        let commands = std::sync::Mutex::new(self.commands.clone());
        let deliver: vision_worker::Deliver = Arc::new(move |message| {
            if let Ok(commands) = commands.lock() {
                let _ = commands.send(Command::Vision(message));
            }
        });
        let allocated_before = self.engine.device_facts().allocated_bytes;
        let worker = VisionWorker::spawn::<B>(dir.join(crate::vision::WEIGHTS), vision.clone(), deliver).map_err(Failure::Engine)?;
        self.vision = Some(VisionRuntime {
            worker,
            last_used: Instant::now(),
            resident_bytes: vision.engine().resident_bytes(),
            allocated_before,
            loaded: false,
        });
        self.publish_view();
        Ok(())
    }

    fn vision_message(&mut self, message: vision_worker::Message) {
        match message {
            vision_worker::Message::Loaded { load_ms } => {
                let allocated = self.engine.device_facts().allocated_bytes;
                if let Some(vision) = self.vision.as_mut() {
                    vision.loaded = true;
                    vision.last_used = Instant::now();
                    // The measured cost replaces the estimate (when the device reports allocations).
                    if let (Some(before), Some(after)) = (vision.allocated_before, allocated)
                        && after > before
                    {
                        vision.resident_bytes = after - before;
                    }
                }
                let resident = self.vision.as_ref().map(|vision| vision.resident_bytes);
                tracing::info!(load_ms, resident_bytes = ?resident, "vision tower loaded");
                (self.publisher)("model.vision", json!({"event": "loaded", "loaded": true, "load_ms": load_ms, "resident_bytes": resident, "allocated_bytes": allocated}));
                self.refresh_device();
                self.publish_view();
            }
            vision_worker::Message::LoadFailed(error) => {
                tracing::error!(%error, "the vision tower failed to load");
                let keys: Vec<String> = self.encoding.drain().collect();
                for key in keys {
                    self.fail_waiting(&key, Failure::Engine(error.clone()));
                }
                if let Some(vision) = self.vision.take() {
                    vision.worker.stop();
                }
                (self.publisher)("model.vision", json!({"event": "failed", "loaded": false, "error": error}));
                self.publish_view();
            }
            vision_worker::Message::Encoded { key, result, preprocess_ms, encode_ms } => {
                self.encoding.remove(&key);
                if let Some(vision) = self.vision.as_mut() {
                    vision.last_used = Instant::now();
                }
                match result {
                    Ok(rows) => {
                        tracing::info!(image = %key, preprocess_ms, encode_ms, "image encoded");
                        (self.publisher)("model.vision", json!({"event": "encoded", "key": key, "preprocess_ms": preprocess_ms, "encode_ms": encode_ms}));
                        for active in &mut self.active {
                            for image in active.images.iter_mut().filter(|image| image.key == key) {
                                image.rows = Some(rows.clone());
                            }
                        }
                        let entry_bytes = rows.len() * 2;
                        while !self.embeddings.is_empty()
                            && self.embeddings.iter().map(|(_, rows)| rows.len() * 2).sum::<usize>() + entry_bytes > EMBEDDING_CACHE_BYTES
                        {
                            self.embeddings.pop_front();
                        }
                        if entry_bytes <= EMBEDDING_CACHE_BYTES {
                            self.embeddings.push_back((key, rows));
                        }
                    }
                    Err(failure) => self.fail_waiting(&key, failure),
                }
                self.publish_view();
            }
        }
    }

    /// Fail every request waiting for the embeddings of `key`.
    fn fail_waiting(&mut self, key: &str, failure: Failure) {
        while let Some(index) =
            self.active.iter().position(|active| active.images.iter().any(|image| image.rows.is_none() && image.key == key))
        {
            self.fail_with(index, failure.clone());
        }
    }

    /// Stop the vision thread: the tower, its scratch and the embedding cache are released.
    fn unload_vision(&mut self) {
        if let Some(vision) = self.vision.take() {
            vision.worker.stop();
            self.embeddings.clear();
            self.engine.trim_buffer_cache();
            self.refresh_device();
            tracing::info!("vision tower unloaded after its idle period");
            (self.publisher)("model.vision", json!({"event": "unloaded", "loaded": false, "allocated_bytes": self.engine.device_facts().allocated_bytes}));
            self.publish_view();
        }
    }

    // ---- reporting ----------------------------------------------------------------------------

    fn refresh_device(&self) {
        if let Ok(mut device) = self.device.lock() {
            *device = self.engine.device_facts().into();
        }
    }

    /// Report the running request at `index` in `phase` when its reporter says it is due.
    fn report(&mut self, index: usize, phase: &'static str) {
        let now = Instant::now();
        let active = &mut self.active[index];
        if !active.reporter.due(phase, now) {
            return;
        }
        let event = Self::progress_of(active, phase, now);
        self.publish_progress(&event);
    }

    fn progress_of(active: &Active<B>, phase: &str, now: Instant) -> cc_protocol::contracts::model::Progress {
        let request = &active.request;
        let mut event = crate::progress::event(request.id, &request.key, request.priority, matches!(request.work, Work::Prewarm), phase, request.prompt.len());
        let read = match active.phase {
            Phase::Prefill { next } => next,
            Phase::Generate(_) => request.prompt.len(),
        };
        let cached = active.cached as usize;
        event.cached_tokens = active.cached;
        event.prefill_total = request.prompt.len().saturating_sub(cached) as u32;
        event.prefill_done = read.saturating_sub(cached).min(event.prefill_total as usize) as u32;
        event.generated = active.generated;
        event.tok_s = crate::progress::tok_s(active.generated, active.first_token, now);
        event.elapsed_ms = now.duration_since(request.arrival).as_secs_f64() * 1000.0;
        event
    }

    /// The last event of a request: `active` when it ran (its counts), `finish` how it ended.
    fn publish_ended(&self, request: &Request, active: Option<&Active<B>>, finish: &str) {
        let now = Instant::now();
        let mut event = match active {
            Some(active) => Self::progress_of(active, progress_phase::DONE, now),
            None => crate::progress::event(request.id, &request.key, request.priority, matches!(request.work, Work::Prewarm), progress_phase::DONE, request.prompt.len()),
        };
        if matches!(request.work, Work::Prewarm) && finish == "prefilled" {
            event.prefill_done = event.prefill_total;
        }
        event.finish = finish.to_owned();
        event.elapsed_ms = now.duration_since(request.arrival).as_secs_f64() * 1000.0;
        self.publish_progress(&event);
    }

    fn publish_progress(&self, event: &cc_protocol::contracts::model::Progress) {
        if let Ok(value) = serde_json::to_value(event) {
            (self.publisher)(model_topics::PROGRESS, value);
        }
    }

    fn publish_view(&self) {
        let mut queued = QueueDepth::default();
        for request in &self.waiting {
            match request.priority {
                Priority::Foreground => queued.foreground += 1,
                Priority::Agent => queued.agent += 1,
                Priority::Background => queued.background += 1,
            }
        }
        let slots = self
            .slots
            .iter()
            .enumerate()
            .map(|(index, slot)| SlotState {
                slot: index as u32,
                cache_key: slot.key.clone(),
                busy: slot.busy.is_some(),
                n_tokens: slot.n_tokens,
                priority: slot.busy,
            })
            .collect();
        let mut hibernated: Vec<String> = self.hibernated.keys().cloned().collect();
        hibernated.sort();
        let view = View {
            slots,
            queued,
            busy: self.slots.iter().filter(|slot| slot.busy.is_some()).count() as u32,
            pool_tokens: self.config.pool_tokens,
            reserved_tokens: self.reserved_tokens(),
            hibernated,
            vision_loaded: self.vision.as_ref().is_some_and(|vision| vision.loaded),
        };
        if let Ok(mut shared) = self.view.lock() {
            *shared = view;
        }
    }
}

/// `index` when `slot` holds `key` (the request's own conversation is never spilled for it), else
/// an index no slot has.
fn slot_index_of_key<B: Backend>(slot: &Slot<B>, key: &str, index: usize) -> usize {
    if slot.key.as_deref() == Some(key) { index } else { usize::MAX }
}

/// A conversation read back from disk: its state, its context checkpoints, its images, and the
/// bytes read.
type Restored<B> = (Box<LanguageModelState<B>>, Vec<Checkpoint<B>>, Vec<ImageSpan>, u64);

enum Placement {
    Admitted,
    Wait,
}

fn rand_seed() -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..8]);
    u64::from_le_bytes(bytes)
}

/// The model's vocabulary size (`config.json`), which can exceed the tokenizer's.
fn model_vocab(dir: &Path) -> Result<usize, String> {
    let text = std::fs::read(dir.join("config.json")).map_err(|error| format!("cannot read config.json: {error}"))?;
    let config: Value = serde_json::from_slice(&text).map_err(|error| format!("cannot parse config.json: {error}"))?;
    config["decoder_config"]["vocab_size"].as_u64().map(|vocab| vocab as usize).ok_or_else(|| "config.json has no vocab_size".to_owned())
}

/// Whether a path is a hibernation file this plugin wrote (used to clean stale files at start).
pub fn is_hibernation_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "state")
}
