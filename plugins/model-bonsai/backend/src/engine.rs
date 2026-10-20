//! The llama.cpp engine on its own thread: the loaded model, its context (one unified KV pool, see
//! `fit.rs`) and the scheduler that runs requests on it. The scheduling is the uzu plugin's, on
//! llama.cpp sequences:
//!
//! - **One sequence per resident conversation.** A slot is a sequence of the context and the
//!   tokens it holds. A request continues its conversation's sequence when that is a prefix of the
//!   new prompt. Else it rolls back to the longest context checkpoint that is (a recurrent state
//!   cannot be cut, only restored), and later checkpoints are then void. Else it starts cold. A
//!   checkpoint is a copy of the sequence's recurrent state (`fit::Geometry::recurrent_bytes`, in host
//!   memory): the attention rows past it are removed, and those before it stay valid.
//! - **Checkpoints at the reuse points.** The prefill stops at the reuse points the runtime found by
//!   rendering the request (the end of the leading system block, the end of the history before the
//!   last message) and before the last prompt token, and keeps a checkpoint there (at most the
//!   fitted number per conversation, least recently used dropped first). A caller that ends every
//!   call with a per-call message so resumes each call at the history the calls share.
//! - **Scheduling.** Every step advances one request by one prefill step (at most 1024 tokens) or
//!   one token, choosing the highest priority class first and rotating within it. Foreground
//!   requests therefore preempt agent work at the next step. Requests beyond the resident
//!   conversations, or for a busy conversation, queue by priority, first come first served within a
//!   class.
//! - **The pool.** Running requests reserve their prompt and generation in the KV pool; when it is
//!   full, the least recently used idle conversation is dropped (it starts cold next time; spilling
//!   to disk comes with hibernation), else the request waits.
//! - **Sampling** on the host from the logits: the grammar's mask, penalties, then greedy or
//!   stochastic (`sampler.rs`); a reasoning block that reaches its budget is closed by feeding Qwen's
//!   own closing sentence.

use crate::fit::{self, Budget, Geometry, Plan};
use crate::grammar::{Grammar, GrammarCompiler, GrammarSpec};
use crate::llama::{self, Context, ContextParams, Gpu, Model};
use crate::sampler::{Sampler, SamplingPlan};
use cc_protocol::contracts::model::{Priority, QueueDepth, SlotState, progress_phase, topics as model_topics};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokenizers::Tokenizer;

/// Prompt tokens per prefill step: it bounds how long a foreground request waits behind another's
/// prefill (the decode call computes it in `fit::MICRO_BATCH` passes).
pub const PREFILL_CHUNK: usize = fit::BATCH as usize;
/// Pool cells reserved for a request's generation when the caller set no `max_tokens`. Admission
/// accounting only; it never limits the output.
pub const GENERATION_RESERVE: u64 = 8192;
/// A request that reads at least this many prompt tokens is logged with what it reused.
const LONG_PREFILL_TOKENS: usize = 4096;

/// Texts whose token ids must be the same in the GGUF vocab and in `tokenizer.json`: chat markup,
/// tool-call markup, code, several scripts, whitespace runs, escapes and the image tokens.
const TOKEN_PROBES: &[&str] = &[
    "<|im_start|>system\nYou are a helpful assistant.<|im_end|>\n<|im_start|>user\nHello<|im_end|>\n<|im_start|>assistant\n<think>\n",
    "<tool_call>\n<function=read_file>\n<parameter=path>\nsrc/main.rs\n</parameter>\n</function>\n</tool_call>",
    "fn main() {\n    let v: Vec<u64> = (0..10).map(|x| x * x).collect();\n}\n",
    "Árvíztűrő tükörfúrógép — naïve café, 東京都, Привет, 🚀 ✓",
    "    \t\t  \n\n\n   trailing   ",
    "{\"route\":\"answer\",\"why\":\"tab\\tnew\\nline\"}",
    "<|vision_start|><|image_pad|><|vision_end|></think>\n\n",
];

pub type Publisher = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// What the engine needs to know at load.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// The GGUF to load.
    pub weights: PathBuf,
    /// `tokenizer.json` beside it.
    pub tokenizer: PathBuf,
    pub physical_bytes: u64,
    /// The operator's memory limit.
    pub memory_limit: Option<u64>,
    /// Minimum context one conversation needs.
    pub min_context: u64,
    /// The operator's override of the fitted pool (tokens).
    pub ctx_total: Option<u64>,
    /// An operator cap on resident conversations (None: the fit's).
    pub max_resident: Option<u64>,
}

/// Facts about the loaded engine.
#[derive(Debug, Clone, Serialize)]
pub struct Loaded {
    pub load_ms: f64,
    pub geometry: Geometry,
    pub plan: Plan,
    /// The pool llama.cpp allocated (tokens).
    pub n_ctx: u32,
    pub n_params: u64,
    /// The GPU before the load and after it (what the load really took).
    pub gpu_before: Option<Gpu>,
    pub gpu_after: Option<Gpu>,
    /// One sequence's recurrent state as llama.cpp reports it (a checkpoint's size).
    pub measured_recurrent_bytes: u64,
    /// End-of-generation tokens.
    pub stop_token_ids: Vec<u32>,
    /// Conversations resident at once (sequences).
    pub resident: u64,
}

/// A reasoning budget: once the open reasoning block has produced `tokens` tokens without closing,
/// the engine feeds `close` (the model's own closing sentence and `</think>`) and the model goes on
/// with its answer. The reasoning is kept; only its end is forced.
#[derive(Debug, Clone, PartialEq)]
pub struct ThinkBudget {
    pub tokens: u32,
    /// The tokens that close reasoning (`</think>`); seeing them ends the budget.
    pub end: Vec<u32>,
    /// What is fed when the budget is reached (ends with `end`).
    pub close: Vec<u32>,
}

#[allow(clippy::large_enum_variant)]
pub enum Work {
    Generate {
        max_tokens: Option<u32>,
        sampling: SamplingPlan,
        grammar: Option<GrammarSpec>,
        /// Engage the grammar only after these tokens were generated (end of thinking).
        grammar_trigger: Option<Vec<u32>>,
        think_budget: Option<ThinkBudget>,
    },
    /// Prefill the prompt (all but its last token) without generating.
    Prewarm,
}

pub struct Request {
    /// Numbered by the engine when it receives the request.
    pub id: u64,
    pub key: String,
    pub prompt: Vec<u32>,
    /// Prompt lengths, besides `prompt.len() - 1`, at which the prefill keeps a checkpoint.
    pub checkpoints: Vec<usize>,
    /// A prewarm of the leading block alone, for conversations not created yet. Kept blocks shared
    /// across conversations come with the next stage; until then it ends at once.
    pub shared_only: bool,
    /// A prewarm that runs only while no other conversation is served: it waits while another
    /// conversation's request runs or waits, and such a request stops it at its next step.
    pub idle_only: bool,
    pub priority: Priority,
    pub work: Work,
    pub events: tokio::sync::mpsc::UnboundedSender<Event>,
    /// Set by the caller to end the request (cancel, stop string, repetition).
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
    Invalid(String),
    Unavailable(String),
    Cancelled,
    Engine(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Finish {
    Stop,
    Length,
    Stopped,
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
    pub slot: u32,
}

pub enum Command {
    Run(Box<Request>),
    /// Keep a conversation resident while its turn runs (`hold`), or let it go: a held one is the
    /// last to be dropped when the pool needs room.
    Hold { key: String, hold: bool },
    /// Drop a conversation for good (its session was deleted): its requests stop and its sequence
    /// is cleared.
    Forget(String),
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
}

pub struct EngineHandle {
    tx: Mutex<mpsc::Sender<Command>>,
    pub tokenizer: Arc<Tokenizer>,
    pub loaded: Loaded,
    view: Arc<Mutex<View>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl EngineHandle {
    pub fn send(&self, command: Command) -> Result<(), String> {
        let tx = self.tx.lock().map_err(|_| "the engine channel is poisoned".to_owned())?;
        tx.send(command).map_err(|_| "the engine stopped".to_owned())
    }

    pub fn view(&self) -> View {
        self.view.lock().map(|view| view.clone()).unwrap_or_default()
    }

    /// Stop the engine thread and release the model (blocks until it has).
    pub fn shutdown(&self) {
        let _ = self.send(Command::Shutdown);
        if let Some(thread) = self.thread.lock().ok().and_then(|mut thread| thread.take()) {
            let _ = thread.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Load the model, fit the context to the machine and start the engine thread.
pub fn start(config: EngineConfig, publisher: Publisher) -> Result<EngineHandle, String> {
    let (tx, rx) = mpsc::channel::<Command>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(Arc<Tokenizer>, Loaded), String>>();
    let view = Arc::new(Mutex::new(View::default()));
    let worker_view = view.clone();
    let thread = std::thread::Builder::new()
        .name("bonsai-engine".into())
        .spawn(move || {
            let started = Instant::now();
            let loaded = (|| -> Result<(Context, Arc<Tokenizer>, GrammarCompiler, Loaded), String> {
                llama::init(2);
                let gpu_before = llama::gpu();
                let json = std::fs::read(&config.tokenizer).map_err(|error| format!("cannot read {}: {error}", config.tokenizer.display()))?;
                let tokenizer = Tokenizer::from_bytes(&json).map_err(|error| format!("cannot read {}: {error}", config.tokenizer.display()))?;
                let model = Model::load(&config.weights)?;
                check_tokenizer(&model, &tokenizer)?;
                let geometry = Geometry::read(&model)?;
                let budget = Budget {
                    working_set: gpu_before.as_ref().map_or(0, |gpu| gpu.working_set_bytes),
                    physical: config.physical_bytes,
                    limit: config.memory_limit,
                };
                let mut plan = fit::fit(&geometry, &budget, config.min_context).ok_or_else(|| {
                    format!(
                        "this machine's memory ({} GiB usable) cannot hold the model ({:.1} GB) with a {}-token conversation",
                        budget.usable() >> 30,
                        geometry.weights_bytes as f64 / 1e9,
                        config.min_context
                    )
                })?;
                if let Some(ctx) = config.ctx_total {
                    plan.ctx_total = ctx;
                }
                let params = ContextParams {
                    n_ctx: u32::try_from(plan.ctx_total).unwrap_or(u32::MAX),
                    n_batch: fit::BATCH,
                    n_ubatch: fit::MICRO_BATCH,
                    n_seq_max: u32::try_from(plan.sequences).unwrap_or(1),
                    kv_type: plan.kv_type,
                };
                let mut context = Context::new(model.clone(), params)?;
                // One token on the first sequence: compiles the kernels before the first request
                // and shows what a recurrent state really takes (a checkpoint's size).
                let first = tokenizer.encode("Hello", false).map_err(|error| format!("tokenizer: {error}"))?.get_ids().to_vec();
                context.decode(&first[..1], 0, 0, false)?;
                let measured_recurrent_bytes = context.state(0, true).len() as u64;
                context.seq_rm(0, -1, -1);
                let n_vocab = model.n_vocab();
                let stop_token_ids: Vec<u32> = (0..n_vocab as u32).filter(|token| model.is_eog(*token)).collect();
                let compiler = GrammarCompiler::new(&json, &stop_token_ids, n_vocab).map_err(|error| format!("cannot prepare grammar support: {error}"))?;
                let resident = config.max_resident.map_or(plan.resident, |cap| cap.clamp(1, plan.resident));
                let loaded = Loaded {
                    load_ms: started.elapsed().as_secs_f64() * 1000.0,
                    n_ctx: context.n_ctx(),
                    geometry,
                    plan,
                    n_params: model.n_params(),
                    gpu_before,
                    gpu_after: llama::gpu(),
                    measured_recurrent_bytes,
                    stop_token_ids,
                    resident,
                };
                Ok((context, Arc::new(tokenizer), compiler, loaded))
            })();
            let (context, compiler, loaded) = match loaded {
                Ok((context, tokenizer, compiler, loaded)) => {
                    let _ = ready_tx.send(Ok((tokenizer, loaded.clone())));
                    (context, compiler, loaded)
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            let mut worker = Worker::new(context, compiler, &loaded, rx, worker_view, publisher);
            worker.run();
        })
        .map_err(|error| format!("cannot start the engine thread: {error}"))?;
    match ready_rx.recv() {
        Ok(Ok((tokenizer, loaded))) => Ok(EngineHandle { tx: Mutex::new(tx), tokenizer, loaded, view, thread: Mutex::new(Some(thread)) }),
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

/// The GGUF vocab and `tokenizer.json` must give the same ids: the template, the output parser and
/// the grammar engine use the second, the model reads the first.
fn check_tokenizer(model: &Model, tokenizer: &Tokenizer) -> Result<(), String> {
    for text in TOKEN_PROBES {
        let theirs = model.tokenize(text);
        let ours = tokenizer.encode(*text, false).map_err(|error| format!("tokenizer: {error}"))?.get_ids().to_vec();
        if theirs != ours {
            return Err(format!("the GGUF vocab and tokenizer.json disagree on {text:?}: {theirs:?} vs {ours:?}"));
        }
    }
    Ok(())
}

// ---- the worker ---------------------------------------------------------------------------------

/// A copy of a sequence's recurrent state when it held `len` tokens.
struct Checkpoint {
    len: usize,
    state: Vec<u8>,
    last_used: Instant,
}

/// One resident conversation: a sequence of the context.
struct Slot {
    seq: u32,
    key: Option<String>,
    /// The tokens the sequence holds.
    tokens: Vec<u32>,
    /// Checkpoints of the sequence, each a prefix of `tokens` (a rollback voids the longer ones).
    checkpoints: Vec<Checkpoint>,
    busy: Option<Priority>,
    last_used: Instant,
}

/// A request that generates.
struct Generation {
    sampler: Sampler,
    grammar: Option<Grammar>,
    /// The token sampled last, not yet handed out or fed.
    pending: u32,
}

enum Phase {
    /// Prompt tokens `[next, end)` remain to be prefilled.
    Prefill { next: usize },
    Generate(Box<Generation>),
}

struct Active {
    slot: usize,
    request: Box<Request>,
    phase: Phase,
    cached: u32,
    queue_ms: f64,
    admitted: Instant,
    reserve: u64,
    first_token: Option<Instant>,
    last_token: Option<Instant>,
    generated: u32,
    /// Tokens handed out so far (with any fed closing tokens).
    emitted: Vec<u32>,
    /// Reasoning tokens produced in the open reasoning block, while a budget applies.
    thinking: Option<u32>,
    reporter: crate::progress::Reporter,
}

enum Placement {
    Admitted,
    Wait,
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

/// How long the engine must have served nothing else before an idle-only generation may take
/// room the way any request does (spill or drop an idle conversation, drop a shared block): until
/// then it uses only free room, so work under way and its caches are never touched.
const IDLE_ROOM_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

/// An idle-only generation (learning after a turn), as opposed to an idle-only prewarm.
fn idle_generation(request: &Request) -> bool {
    request.idle_only && !matches!(request.work, Work::Prewarm)
}

struct Worker {
    context: Context,
    compiler: GrammarCompiler,
    rx: mpsc::Receiver<Command>,
    slots: Vec<Slot>,
    active: Vec<Active>,
    waiting: VecDeque<Box<Request>>,
    held: HashMap<String, Instant>,
    view: Arc<Mutex<View>>,
    publisher: Publisher,
    /// Round-robin position per priority class.
    turn: [usize; 3],
    pool_tokens: u64,
    checkpoints: usize,
    stop_tokens: Vec<u32>,
    shutting_down: bool,
    next_id: u64,
    logits: Vec<f32>,
    /// When the engine last had other work than idle-only generations ([`IDLE_ROOM_AFTER`]).
    last_work: Instant,
    /// The idle-only request whose wait for room was last logged.
    idle_wait_logged: Option<u64>,
}

impl Worker {
    fn new(context: Context, compiler: GrammarCompiler, loaded: &Loaded, rx: mpsc::Receiver<Command>, view: Arc<Mutex<View>>, publisher: Publisher) -> Self {
        let slots = (0..loaded.resident)
            .map(|seq| Slot { seq: seq as u32, key: None, tokens: Vec::new(), checkpoints: Vec::new(), busy: None, last_used: Instant::now() })
            .collect();
        let worker = Self {
            context,
            compiler,
            rx,
            slots,
            active: Vec::new(),
            waiting: VecDeque::new(),
            held: HashMap::new(),
            view,
            publisher,
            turn: [0; 3],
            pool_tokens: u64::from(loaded.n_ctx),
            checkpoints: usize::try_from(loaded.plan.checkpoints).unwrap_or(1),
            stop_tokens: loaded.stop_token_ids.clone(),
            shutting_down: false,
            next_id: 1,
            logits: Vec::new(),
            last_work: Instant::now(),
            idle_wait_logged: None,
        };
        worker.publish_view();
        worker
    }

    fn run(&mut self) {
        let mut stalled = false;
        loop {
            // Block for a command only when nothing runs and nothing waiting can start.
            let idle = self.active.is_empty() && (self.waiting.is_empty() || stalled);
            // An idle-only generation waiting for the quiet period wakes the worker when it ends.
            let wake = self.waiting.iter().any(|request| idle_generation(request)).then(|| self.last_work + IDLE_ROOM_AFTER);
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
                break;
            }
            self.admit();
            stalled = self.active.is_empty() && !self.waiting.is_empty();
            if !self.active.is_empty() {
                self.step();
            }
        }
        self.active.clear();
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Run(mut request) => {
                if request.prompt.is_empty() {
                    let _ = request.events.send(Event::Failed(Failure::Invalid("the prompt is empty".into())));
                    return;
                }
                if request.prompt.len() as u64 >= self.pool_tokens {
                    let _ = request.events.send(Event::Failed(Failure::Invalid(format!(
                        "the prompt has {} tokens; this load holds at most {} per conversation",
                        request.prompt.len(),
                        self.pool_tokens
                    ))));
                    return;
                }
                request.id = self.next_id;
                self.next_id += 1;
                let ahead = self.waiting.iter().filter(|other| class(other.priority) <= class(request.priority)).count();
                let mut event = crate::progress::event(request.id, &request.key, request.priority, matches!(request.work, Work::Prewarm), progress_phase::QUEUED, request.prompt.len());
                event.ahead = ahead as u32;
                self.publish_progress(&event);
                self.waiting.push_back(request);
            }
            Command::Hold { key, hold } => {
                if hold {
                    self.held.insert(key, Instant::now());
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
                if let Some(index) = self.slots.iter().position(|slot| slot.key.as_deref() == Some(key.as_str()) && slot.busy.is_none()) {
                    self.clear_slot(index);
                }
                self.publish_view();
            }
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

    fn reserved_tokens(&self) -> u64 {
        self.active.iter().map(|active| active.reserve).sum()
    }

    /// Cells the pool holds: every sequence's tokens and the running requests' reserves.
    fn pool_used(&self) -> u64 {
        self.slots.iter().map(|slot| slot.tokens.len() as u64).sum::<u64>() + self.reserved_tokens()
    }

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
        // Drop requests whose caller is gone before they start, and prewarms made moot by a real
        // request for the same conversation.
        let mut index = 0;
        while index < self.waiting.len() {
            let request = &self.waiting[index];
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
                if let Placement::Admitted = self.place(position) {
                    admitted = true;
                    break;
                }
            }
            if !admitted {
                break;
            }
        }
        self.publish_view();
    }

    fn place(&mut self, position: usize) -> Placement {
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
        // A prewarm of a leading block for conversations not created yet: nothing keeps such a
        // block at this stage, so it is done at once.
        if self.waiting[position].shared_only {
            let Some(request) = self.waiting.remove(position) else { return Placement::Wait };
            self.publish_ended(&request, None, "prefilled");
            let _ = request.events.send(Event::Done(Outcome {
                finish: Finish::Prefilled,
                prompt_tokens: request.prompt.len() as u32,
                cached_tokens: 0,
                completion_tokens: 0,
                queue_ms: ms(request.arrival),
                prefill_ms: 0.0,
                decode_ms: 0.0,
                slot: 0,
            }));
            return Placement::Admitted;
        }
        let priority = self.waiting[position].priority;
        let blocked_by_higher = self
            .waiting
            .iter()
            .enumerate()
            .any(|(index, other)| index != position && class(other.priority) < class(priority) && !self.key_busy(&other.key));
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
                match self.free_slot() {
                    Some(index) => index,
                    None => {
                        self.preempt_prewarm(&key, priority, false);
                        return Placement::Wait;
                    }
                }
            }
        };
        // Room in the pool: drop idle conversations (least recently used, held ones last).
        while self.pool_used() + reserve > self.pool_tokens {
            let candidate = self
                .slots
                .iter()
                .enumerate()
                .filter(|(index, other)| *index != slot && other.busy.is_none() && !other.tokens.is_empty())
                .min_by_key(|(_, other)| (other.key.as_ref().is_some_and(|key| self.held.contains_key(key)), other.last_used))
                .map(|(index, _)| index);
            match candidate {
                Some(index) => {
                    tracing::info!(cache_key = ?self.slots[index].key, tokens = self.slots[index].tokens.len(), "dropping an idle conversation for room in the pool");
                    self.clear_slot(index);
                }
                None => break,
            }
        }
        if self.pool_used() + reserve > self.pool_tokens && !self.active.is_empty() {
            return Placement::Wait;
        }
        let Some(request) = self.waiting.remove(position) else { return Placement::Wait };
        self.start(slot, request, reserve);
        Placement::Admitted
    }

    /// An idle-only generation (learning after a turn) takes nothing from anyone: it starts only
    /// while no other request runs or waits, on its own conversation's slot or an empty one, in the
    /// room the pool has (no conversation is dropped for it). Once running, the scheduling class
    /// puts every other request first.
    fn place_idle(&mut self, position: usize, key: &str) -> Placement {
        let others = self.active.iter().any(|active| active.request.key != key) || self.waiting.iter().enumerate().any(|(index, other)| index != position && !other.idle_only);
        let slot = self.slots.iter().position(|slot| slot.busy.is_none() && slot.key.as_deref() == Some(key)).or_else(|| self.slots.iter().position(|slot| slot.busy.is_none() && slot.key.is_none()));
        let reserve = self.reserve_for(&self.waiting[position]);
        let Some(slot) = slot else { return Placement::Wait };
        if others || self.pool_used() + reserve > self.pool_tokens {
            if !others && self.idle_wait_logged != Some(self.waiting[position].id) {
                self.idle_wait_logged = Some(self.waiting[position].id);
                tracing::info!(pool_used = self.pool_used(), reserve, pool = self.pool_tokens, "an idle-only request waits for room in the pool");
            }
            return Placement::Wait;
        }
        let Some(request) = self.waiting.remove(position) else { return Placement::Wait };
        self.start(slot, request, reserve);
        Placement::Admitted
    }

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

    fn serving_other(&self, key: &str) -> bool {
        let real = |request: &Request| request.key != key && !matches!(request.work, Work::Prewarm);
        self.active.iter().any(|active| real(&active.request)) || self.waiting.iter().any(|request| real(request))
    }

    fn stop_idle_prewarms(&mut self, key: &str) {
        for active in self.active.iter().filter(|active| active.request.idle_only && matches!(active.request.work, Work::Prewarm) && active.request.key != key) {
            active.request.stop.store(true, Ordering::SeqCst);
        }
    }

    fn key_busy(&self, key: &str) -> bool {
        self.slots.iter().any(|slot| slot.key.as_deref() == Some(key) && slot.busy.is_some())
    }

    /// A slot for a new conversation: an empty one, else the least recently used idle one (held
    /// ones last). None while every slot is busy.
    fn free_slot(&self) -> Option<usize> {
        if let Some(index) = self.slots.iter().position(|slot| slot.busy.is_none() && slot.key.is_none()) {
            return Some(index);
        }
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.busy.is_none())
            .min_by_key(|(_, slot)| (slot.key.as_ref().is_some_and(|key| self.held.contains_key(key)), slot.last_used))
            .map(|(index, _)| index)
    }

    /// Pool cells a request may add: its prompt beyond what its conversation holds, plus its
    /// generation.
    fn reserve_for(&self, request: &Request) -> u64 {
        let generation = match &request.work {
            Work::Generate { max_tokens, .. } => max_tokens.map_or(GENERATION_RESERVE, u64::from),
            Work::Prewarm => 0,
        };
        let held = self.slots.iter().find(|slot| slot.key.as_deref() == Some(request.key.as_str())).map_or(0, |slot| slot.tokens.len() as u64);
        (request.prompt.len() as u64).saturating_sub(held).saturating_add(generation).min(self.pool_tokens)
    }

    fn clear_slot(&mut self, index: usize) {
        let seq = self.slots[index].seq;
        self.context.seq_rm(seq, -1, -1);
        let slot = &mut self.slots[index];
        slot.key = None;
        slot.tokens.clear();
        slot.checkpoints.clear();
    }

    /// Put `request` on `slot`: continue its conversation's sequence, roll it back to a checkpoint,
    /// or start cold, and decide how much of the prompt is already processed.
    fn start(&mut self, slot_index: usize, request: Box<Request>, reserve: u64) {
        let queue_ms = ms(request.arrival);
        if self.slots[slot_index].key.as_deref() != Some(request.key.as_str()) {
            self.clear_slot(slot_index);
            self.slots[slot_index].key = Some(request.key.clone());
        }
        let prompt = &request.prompt;
        let slot = &self.slots[slot_index];
        // At least the last prompt token is always fed: its forward pass gives the first logits.
        let common = slot.tokens.iter().zip(prompt.iter()).take_while(|(a, b)| a == b).count();
        let usable = common.min(prompt.len() - 1);
        let cached = if usable == slot.tokens.len() {
            usable
        } else {
            let best = slot
                .checkpoints
                .iter()
                .enumerate()
                .filter(|(_, checkpoint)| checkpoint.len > 0 && checkpoint.len <= usable)
                .max_by_key(|(_, checkpoint)| checkpoint.len)
                .map(|(position, _)| position);
            let seq = slot.seq;
            let restored = best.and_then(|position| {
                let checkpoint = &self.slots[slot_index].checkpoints[position];
                let len = checkpoint.len;
                (self.context.set_state(seq, true, &checkpoint.state) && self.context.seq_rm(seq, len as i32, -1)).then_some((position, len))
            });
            match restored {
                Some((position, len)) => {
                    let slot = &mut self.slots[slot_index];
                    slot.checkpoints[position].last_used = Instant::now();
                    // The rows after the rollback point will be rewritten: longer checkpoints are void.
                    slot.checkpoints.retain(|checkpoint| checkpoint.len <= len);
                    slot.tokens.truncate(len);
                    len
                }
                None => {
                    self.context.seq_rm(seq, -1, -1);
                    let slot = &mut self.slots[slot_index];
                    slot.tokens.clear();
                    slot.checkpoints.clear();
                    0
                }
            }
        };
        let slot = &mut self.slots[slot_index];
        slot.last_used = Instant::now();
        slot.busy = Some(request.priority);
        let to_read = request.prompt.len().saturating_sub(cached);
        if to_read >= LONG_PREFILL_TOKENS {
            tracing::info!(cache_key = %request.key, prompt_tokens = request.prompt.len(), reused_tokens = cached, to_read, prewarm = matches!(request.work, Work::Prewarm), "reading a long prompt");
        }
        let _ = request.events.send(Event::Started { slot: slot_index as u32, queue_ms, cached_tokens: cached as u32, prompt_tokens: request.prompt.len() as u32 });
        let thinking = match &request.work {
            Work::Generate { think_budget: Some(_), .. } => Some(0),
            _ => None,
        };
        self.active.push(Active {
            slot: slot_index,
            request,
            phase: Phase::Prefill { next: cached },
            cached: cached as u32,
            queue_ms,
            admitted: Instant::now(),
            reserve,
            first_token: None,
            last_token: None,
            generated: 0,
            emitted: Vec::new(),
            thinking,
            reporter: crate::progress::Reporter::default(),
        });
        let index = self.active.len() - 1;
        self.report(index, progress_phase::PREFILL);
    }

    // ---- stepping -----------------------------------------------------------------------------

    fn step(&mut self) {
        // Stopped requests end first (cancellation is prompt).
        if let Some(index) = self.active.iter().position(|active| active.request.stop.load(Ordering::SeqCst) || active.request.events.is_closed()) {
            self.finish(index, Finish::Stopped);
            return;
        }
        let Some(best) = self.active.iter().map(|active| class(active.request.priority)).min() else { return };
        let candidates: Vec<usize> = (0..self.active.len()).filter(|&index| class(self.active[index].request.priority) == best).collect();
        let turn = &mut self.turn[best];
        *turn = (*turn + 1) % candidates.len().max(1);
        let index = candidates[*turn % candidates.len()];
        if let Err(error) = self.advance(index) {
            tracing::warn!(%error, "a request failed in the engine");
            self.fail(index, error);
        }
    }

    fn advance(&mut self, index: usize) -> Result<(), String> {
        let slot_index = self.active[index].slot;
        let seq = self.slots[slot_index].seq;
        let start = match &self.active[index].phase {
            Phase::Prefill { next } => *next,
            Phase::Generate(_) => return self.decode_step(index),
        };
        let end = self.active[index].request.prompt.len() - 1;
        if start < end {
            // Up to the next reuse point (checkpointed when reached).
            let reuse = self.active[index].request.checkpoints.iter().copied().filter(|&point| point > start && point < end).min();
            let stop = (start + PREFILL_CHUNK).min(end).min(reuse.unwrap_or(usize::MAX));
            let chunk = self.active[index].request.prompt[start..stop].to_vec();
            self.context.decode(&chunk, start as u32, seq, false)?;
            self.slots[slot_index].tokens.extend_from_slice(&chunk);
            if let Phase::Prefill { next } = &mut self.active[index].phase {
                *next = stop;
            }
            if reuse == Some(stop) {
                self.checkpoint(slot_index, stop);
            }
            self.report(index, progress_phase::PREFILL);
            return Ok(());
        }
        // Everything but the last token is in the sequence: checkpoint it, then generate.
        self.checkpoint(slot_index, end);
        if matches!(self.active[index].request.work, Work::Prewarm) {
            self.finish(index, Finish::Prefilled);
            return Ok(());
        }
        let last = self.active[index].request.prompt[end];
        self.context.decode(&[last], end as u32, seq, true)?;
        self.slots[slot_index].tokens.push(last);
        let Work::Generate { sampling, grammar, grammar_trigger, .. } = &self.active[index].request.work else { return Ok(()) };
        let grammar = match grammar {
            Some(spec) => Some(self.compiler.compile(spec, grammar_trigger.clone()).map_err(|error| format!("the grammar does not compile: {error}"))?),
            None => None,
        };
        let sampler = Sampler::new(sampling.clone(), &self.active[index].request.prompt);
        let mut generation = Box::new(Generation { sampler, grammar, pending: 0 });
        generation.pending = self.sample(&mut generation)?;
        self.active[index].phase = Phase::Generate(generation);
        Ok(())
    }

    /// Sample the next token from the context's last logits and note it with the grammar.
    fn sample(&mut self, generation: &mut Generation) -> Result<u32, String> {
        self.logits.clear();
        self.logits.extend_from_slice(self.context.logits());
        if self.logits.is_empty() {
            return Err("no logits after the decode".into());
        }
        if let Some(grammar) = generation.grammar.as_mut() {
            grammar.apply(&mut self.logits);
        }
        let token = generation.sampler.sample(&mut self.logits).ok_or("no token is allowed (the grammar admits nothing here)")?;
        if let Some(grammar) = generation.grammar.as_mut() {
            grammar.accept(token)?;
        }
        Ok(token)
    }

    /// Hand out the pending token, feed it, sample the next.
    fn decode_step(&mut self, index: usize) -> Result<(), String> {
        let slot_index = self.active[index].slot;
        let seq = self.slots[slot_index].seq;
        let Phase::Generate(generation) = &self.active[index].phase else { return Ok(()) };
        let token = generation.pending;
        let now = Instant::now();
        let active = &mut self.active[index];
        active.first_token.get_or_insert(now);
        active.last_token = Some(now);
        if self.stop_tokens.contains(&token) {
            self.finish(index, Finish::Stop);
            return Ok(());
        }
        let active = &mut self.active[index];
        active.generated += 1;
        let _ = active.request.events.send(Event::Token(token));
        active.emitted.push(token);
        let pos = self.slots[slot_index].tokens.len() as u32;
        self.context.decode(&[token], pos, seq, true)?;
        self.slots[slot_index].tokens.push(token);
        self.report(index, progress_phase::DECODE);
        let active = &mut self.active[index];
        let max_tokens = match &active.request.work {
            Work::Generate { max_tokens, .. } => *max_tokens,
            Work::Prewarm => None,
        };
        if max_tokens.is_some_and(|max| active.generated >= max) {
            self.finish(index, Finish::Length);
            return Ok(());
        }
        // The reasoning budget.
        if let (Some(used), Work::Generate { think_budget: Some(budget), .. }) = (active.thinking, &active.request.work) {
            if active.emitted.ends_with(&budget.end) {
                active.thinking = None;
            } else if used + 1 >= budget.tokens {
                return self.close_thinking(index);
            } else {
                active.thinking = Some(used + 1);
            }
        }
        let Phase::Generate(mut generation) = std::mem::replace(&mut self.active[index].phase, Phase::Prefill { next: 0 }) else { return Ok(()) };
        generation.sampler.push(token);
        let next = self.sample(&mut generation);
        self.active[index].phase = Phase::Generate(generation);
        if let Phase::Generate(generation) = &mut self.active[index].phase {
            generation.pending = next?;
        }
        Ok(())
    }

    /// The reasoning budget is reached: feed the closing tokens (handed out like generated ones, so
    /// the output parser sees reasoning end) and go on with the answer on the same sequence.
    fn close_thinking(&mut self, index: usize) -> Result<(), String> {
        let Work::Generate { think_budget: Some(budget), .. } = &self.active[index].request.work else { return Ok(()) };
        let close = budget.close.clone();
        let slot_index = self.active[index].slot;
        let seq = self.slots[slot_index].seq;
        tracing::info!(reasoning_tokens = self.active[index].thinking.unwrap_or(0) + 1, "reasoning budget reached; ending the reasoning block");
        let active = &mut self.active[index];
        active.thinking = None;
        for token in &close {
            let _ = active.request.events.send(Event::Token(*token));
        }
        active.emitted.extend_from_slice(&close);
        active.generated += close.len() as u32;
        let pos = self.slots[slot_index].tokens.len() as u32;
        self.context.decode(&close, pos, seq, true)?;
        self.slots[slot_index].tokens.extend_from_slice(&close);
        let Phase::Generate(mut generation) = std::mem::replace(&mut self.active[index].phase, Phase::Prefill { next: 0 }) else { return Ok(()) };
        for token in &close {
            generation.sampler.push(*token);
        }
        // The answer starts now: a grammar waiting for the end of reasoning applies from here.
        if let Some(grammar) = generation.grammar.as_mut() {
            grammar.engage_now();
        }
        let next = self.sample(&mut generation);
        self.active[index].phase = Phase::Generate(generation);
        if let Phase::Generate(generation) = &mut self.active[index].phase {
            generation.pending = next?;
        }
        Ok(())
    }

    /// Keep a checkpoint of the slot's sequence at `len` tokens (when it holds exactly that many
    /// and none is kept there yet), evicting the least recently used beyond the fitted count.
    fn checkpoint(&mut self, slot_index: usize, len: usize) {
        let slot = &self.slots[slot_index];
        if self.checkpoints == 0 || len == 0 || slot.tokens.len() != len || slot.checkpoints.iter().any(|checkpoint| checkpoint.len == len) {
            return;
        }
        let state = self.context.state(slot.seq, true);
        if state.is_empty() {
            return;
        }
        let checkpoints = &mut self.slots[slot_index].checkpoints;
        checkpoints.push(Checkpoint { len, state, last_used: Instant::now() });
        while checkpoints.len() > self.checkpoints {
            let Some(oldest) = checkpoints.iter().enumerate().min_by_key(|(_, checkpoint)| checkpoint.last_used).map(|(position, _)| position) else { break };
            checkpoints.remove(oldest);
        }
    }

    fn release(&mut self, index: usize) -> Active {
        let active = self.active.remove(index);
        let slot = &mut self.slots[active.slot];
        slot.busy = None;
        slot.last_used = Instant::now();
        active
    }

    fn finish(&mut self, index: usize, finish: Finish) {
        let active = self.release(index);
        // An idle-only generation leaves nothing in the pool.
        if active.request.idle_only && !matches!(active.request.work, Work::Prewarm) {
            self.clear_slot(active.slot);
        }
        let prefill_ms = active.first_token.map_or(ms(active.admitted), |first| first.duration_since(active.admitted).as_secs_f64() * 1000.0);
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
        self.publish_view();
    }

    fn fail(&mut self, index: usize, error: String) {
        let active = self.release(index);
        // The sequence may be inconsistent after an engine error: start that conversation cold.
        self.clear_slot(active.slot);
        self.publish_ended(&active.request, Some(&active), "failed");
        let _ = active.request.events.send(Event::Failed(Failure::Engine(error)));
        self.publish_view();
    }

    // ---- progress -----------------------------------------------------------------------------

    fn report(&mut self, index: usize, phase: &'static str) {
        let now = Instant::now();
        let active = &mut self.active[index];
        if !active.reporter.due(phase, now) {
            return;
        }
        let event = Self::progress_of(active, phase, now);
        self.publish_progress(&event);
    }

    fn progress_of(active: &Active, phase: &str, now: Instant) -> cc_protocol::contracts::model::Progress {
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

    fn publish_ended(&self, request: &Request, active: Option<&Active>, finish: &str) {
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
            .map(|(index, slot)| SlotState { slot: index as u32, cache_key: slot.key.clone(), busy: slot.busy.is_some(), n_tokens: slot.tokens.len() as u32, priority: slot.busy })
            .collect();
        let view = View {
            slots,
            queued,
            busy: self.slots.iter().filter(|slot| slot.busy.is_some()).count() as u32,
            pool_tokens: self.pool_tokens,
            reserved_tokens: self.reserved_tokens(),
        };
        if let Ok(mut shared) = self.view.lock() {
            *shared = view;
        }
    }
}
