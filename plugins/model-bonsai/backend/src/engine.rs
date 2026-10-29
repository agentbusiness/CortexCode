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
//!   call with a per-call message so resumes each call at the history the calls share. The
//!   shortest checkpoint (the end of the leading system block) is never the one evicted: a later
//!   turn's prompt differs after the history, so it is the point every next call can resume from.
//! - **Shared leading blocks.** Conversations of one project start with the same system block
//!   (system prompt, tools). The prefill that reaches a prompt's `shared` point keeps the block on
//!   a sequence of its own (a [`Seed`]): its attention rows are shared with the conversation (no
//!   copy; llama.cpp counts a cell once for every sequence on it), its recurrent state is the
//!   checkpoint there. A request whose conversation holds less of its prompt starts from a copy
//!   of it, so a new conversation reads only what follows the block. A seed is saved to disk
//!   (`seeds.rs`) and comes back from there after a restart or for another project's block.
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
//! - **Drafts** (a file with an MTP layer): each generation step, the MTP head guesses the token
//!   after the one being fed, and both go through the model in one pass. The token sampled from
//!   the first row is the real next token; when it equals the guess, the second row's logits give
//!   the token after it, so the step yields two tokens. Else the guess is removed again (a
//!   recurrent-state snapshot, `n_rs_seq`). The output is exactly what plain decoding samples.
//!   The draft context holds the MTP layer's own cache for every sequence: each row pairs a token
//!   with the model's hidden state at the position before it, fed as the model reads them.
//! - **Images** (`vision.rs`). An image is a run of rows in the prompt (one `<|image_pad|>` token
//!   each, told apart by the image's identity), fed as the encoder's embeddings. With Qwen's M-RoPE
//!   an image advances the positions by fewer than its rows, so a sequence's next position is
//!   counted from its images ([`position`]). A request whose next rows are an image not encoded
//!   yet waits for the vision thread while the others run; images already in the conversation's
//!   sequence are never encoded again.

use crate::fit::{self, Budget, Geometry, Plan};
use crate::grammar::{Grammar, GrammarCompiler, GrammarSpec};
use crate::llama::{self, Context, ContextParams, Gpu, Logits, Model};
use crate::sampler::{Sampler, SamplingPlan};
use crate::vision::{Encoded, VisionHandle};
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
    /// The variant's vision projector (loaded on first use, once installed) and the pixel budget
    /// per image; None: text only.
    pub vision: Option<(PathBuf, u64)>,
    /// Where shared leading blocks are saved (None: kept in memory only).
    pub seeds_dir: Option<PathBuf>,
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
    /// Tokens the MTP head drafts per step (0: the file has no MTP layer).
    pub draft_tokens: u32,
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

/// An image of a prompt: `rows` tokens from `start`, fed as the image's embeddings.
#[derive(Debug, Clone)]
pub struct PromptImage {
    pub start: usize,
    pub rows: usize,
    /// Positions the image advances the sequence by.
    pub n_pos: u32,
    /// The image and its size (two images are the same input only when this matches).
    pub key: u64,
    pub path: PathBuf,
    pub sha256: String,
}

pub struct Request {
    /// Numbered by the engine when it receives the request.
    pub id: u64,
    pub key: String,
    pub prompt: Vec<u32>,
    /// The prompt's images, in order (their rows are `<|image_pad|>` tokens in `prompt`).
    pub images: Vec<PromptImage>,
    /// Prompt lengths, besides `prompt.len() - 1`, at which the prefill keeps a checkpoint.
    pub checkpoints: Vec<usize>,
    /// The prompt length that ends its leading system block (one of `checkpoints`): what
    /// conversations of a project share ([`Seed`]).
    pub shared: Option<usize>,
    /// A prewarm of the leading block alone, for conversations not created yet: it ends kept as a
    /// [`Seed`], and no conversation has its key.
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
    /// Drafted tokens and how many of them the model kept.
    pub draft_total: u32,
    pub draft_accepted: u32,
}

pub enum Command {
    Run(Box<Request>),
    /// Keep a conversation resident while its turn runs (`hold`), or let it go: a held one is the
    /// last to be dropped when the pool needs room.
    Hold { key: String, hold: bool },
    /// Drop a conversation for good (its session was deleted): its requests stop and its sequence
    /// is cleared.
    Forget(String),
    /// The vision thread encoded image `image` of request `request`.
    Encoded { request: u64, image: usize, result: Result<Arc<Encoded>, String> },
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
    /// Image input (None: the variant has no vision projector).
    pub vision: Option<Arc<VisionHandle>>,
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

    /// Stop the engine thread and the vision thread and release the model (blocks until both have).
    pub fn shutdown(&self) {
        let _ = self.send(Command::Shutdown);
        if let Some(thread) = self.thread.lock().ok().and_then(|mut thread| thread.take()) {
            let _ = thread.join();
        }
        if let Some(vision) = &self.vision {
            vision.shutdown();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What a load hands the engine thread: the model's context, the drafter, the tokenizer, the
/// grammar compiler, the facts and the vision thread.
type LoadedEngine = (Context, Option<Drafter>, Arc<Tokenizer>, GrammarCompiler, Loaded, Option<Arc<VisionHandle>>);

/// Load the model, fit the context to the machine and start the engine thread.
pub fn start(config: EngineConfig, publisher: Publisher) -> Result<EngineHandle, String> {
    let (tx, rx) = mpsc::channel::<Command>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(Arc<Tokenizer>, Loaded, Option<Arc<VisionHandle>>), String>>();
    let encoded_tx = tx.clone();
    let view = Arc::new(Mutex::new(View::default()));
    let worker_view = view.clone();
    let thread = std::thread::Builder::new()
        .name("bonsai-engine".into())
        .spawn(move || {
            let started = Instant::now();
            let loaded = (|| -> Result<LoadedEngine, String> {
                llama::init(2);
                let gpu_before = llama::gpu();
                let json = std::fs::read(&config.tokenizer).map_err(|error| format!("cannot read {}: {error}", config.tokenizer.display()))?;
                let tokenizer = Tokenizer::from_bytes(&json).map_err(|error| format!("cannot read {}: {error}", config.tokenizer.display()))?;
                let model = Model::load(&config.weights)?;
                let drafts = model.n_layer_nextn() > 0;
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
                let drafts = drafts && plan.draft_tokens > 0;
                let params = ContextParams {
                    n_ctx: u32::try_from(plan.ctx_total).unwrap_or(u32::MAX),
                    n_batch: fit::BATCH,
                    n_ubatch: fit::MICRO_BATCH,
                    n_seq_max: u32::try_from(plan.sequences).unwrap_or(1),
                    kv_type: plan.kv_type,
                    n_rs_seq: if drafts { plan.draft_tokens } else { 0 },
                    mtp: false,
                };
                let mut context = Context::new(model.clone(), params)?;
                let drafter = if drafts {
                    // The model hands out its hidden state for every row it reads (the MTP rows).
                    context.set_embeddings_nextn(true, false);
                    let mut draft_context = Context::new(model.clone(), ContextParams { n_rs_seq: 0, mtp: true, ..params })?;
                    draft_context.set_embeddings_nextn(true, true);
                    Some(Drafter::new(draft_context, model.n_embd_out(), plan.sequences as usize))
                } else {
                    None
                };
                // One token on the first sequence: compiles the kernels before the first request
                // and shows what a recurrent state really takes (a checkpoint's size).
                let first = tokenizer.encode("Hello", false).map_err(|error| format!("tokenizer: {error}"))?.get_ids().to_vec();
                context.decode(&first[..1], 0, 0, false)?;
                let measured_recurrent_bytes = context.state(0, true).len() as u64;
                context.seq_rm(0, -1, -1);
                let mut drafter = drafter;
                if let Some(drafter) = drafter.as_mut() {
                    drafter.warm_up(first[0])?;
                }
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
                    draft_tokens: if drafter.is_some() { plan.draft_tokens } else { 0 },
                };
                let vision = match config.vision.clone() {
                    Some((mmproj, max_pixels)) => Some(Arc::new(VisionHandle::start(mmproj, model.clone(), max_pixels)?)),
                    None => None,
                };
                Ok((context, drafter, Arc::new(tokenizer), compiler, loaded, vision))
            })();
            let (context, drafter, compiler, loaded, vision) = match loaded {
                Ok((context, drafter, tokenizer, compiler, loaded, vision)) => {
                    let _ = ready_tx.send(Ok((tokenizer, loaded.clone(), vision.clone())));
                    (context, drafter, compiler, loaded, vision)
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            let store = config.seeds_dir.clone().map(|dir| crate::seeds::Store::new(dir, &config.weights, loaded.plan.kv_type));
            let mut worker = Worker::new(context, drafter, compiler, &loaded, rx, worker_view, publisher, vision, encoded_tx, store);
            worker.run();
        })
        .map_err(|error| format!("cannot start the engine thread: {error}"))?;
    match ready_rx.recv() {
        Ok(Ok((tokenizer, loaded, vision))) => Ok(EngineHandle { tx: Mutex::new(tx), tokenizer, loaded, vision, view, thread: Mutex::new(Some(thread)) }),
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

// ---- the drafter ------------------------------------------------------------------------------

/// What the draft context needs from one sequence of the model: the rows the model read that the
/// draft context has not (they go in with the next draft, or once a batch's worth is waiting), and
/// the model's hidden state at the sequence's last position, which the next row pairs with.
#[derive(Default)]
struct Lane {
    /// Position of the first waiting row.
    pos0: u32,
    tokens: Vec<u32>,
    /// One hidden state per waiting row: the model's, at the position before the row's token.
    hidden: Vec<f32>,
    /// The model's hidden state at the last position it read (zeros before the first token).
    last: Vec<f32>,
    /// The draft context failed for this sequence: no drafts until it starts over.
    off: bool,
}

/// The MTP draft context and one lane per sequence of the model's context.
struct Drafter {
    context: Context,
    n_embd: usize,
    lanes: Vec<Lane>,
}

impl Drafter {
    fn new(context: Context, n_embd: usize, sequences: usize) -> Self {
        let lanes = (0..sequences).map(|_| Lane { last: vec![0.0; n_embd], ..Lane::default() }).collect();
        Self { context, n_embd, lanes }
    }

    /// One draft row on the first sequence: compiles the head's kernels before the first request.
    fn warm_up(&mut self, token: u32) -> Result<(), String> {
        let row = vec![0.0f32; self.n_embd];
        self.context.decode_rows(&[token], &row, self.n_embd, 0, 0, true)?;
        self.context.seq_rm(0, -1, -1);
        Ok(())
    }

    /// The model read `tokens` at `pos0..` on `seq`; `hidden` holds its hidden state for each.
    fn note(&mut self, seq: u32, tokens: &[u32], pos0: u32, hidden: &[f32]) {
        let n_embd = self.n_embd;
        let full = {
            let lane = &self.lanes[seq as usize];
            !lane.off && lane.tokens.len() + tokens.len() > fit::BATCH as usize
        };
        if full {
            self.flush(seq);
        }
        let lane = &mut self.lanes[seq as usize];
        if lane.off {
            return;
        }
        if lane.tokens.is_empty() {
            lane.pos0 = pos0;
        }
        for (index, token) in tokens.iter().enumerate() {
            lane.tokens.push(*token);
            // The row pairs the token with the hidden state at the position before it.
            if index == 0 {
                lane.hidden.extend_from_slice(&lane.last);
            } else {
                lane.hidden.extend_from_slice(&hidden[(index - 1) * n_embd..index * n_embd]);
            }
        }
        if let Some(last) = hidden.chunks_exact(n_embd).last() {
            lane.last.copy_from_slice(last);
        }
    }

    /// Put the waiting rows of `seq` into the draft context.
    fn flush(&mut self, seq: u32) {
        let lane = &mut self.lanes[seq as usize];
        if lane.off || lane.tokens.is_empty() {
            return;
        }
        if let Err(error) = self.context.decode_rows(&lane.tokens, &lane.hidden, self.n_embd, lane.pos0, seq, false) {
            tracing::warn!(%error, "the draft context failed; this conversation goes on without drafts");
            lane.off = true;
        }
        lane.tokens.clear();
        lane.hidden.clear();
    }

    /// Guess the token after `token`, which the model reads next at `pos` on `seq`: the waiting
    /// rows and the guess's own row go in one decode, and the guess's row is removed again (it
    /// comes back with the model's rows once the model has read the token).
    fn draft(&mut self, seq: u32, token: u32, pos: u32) -> Option<u32> {
        if self.lanes[seq as usize].off {
            return None;
        }
        if self.lanes[seq as usize].tokens.len() + 1 > fit::BATCH as usize {
            self.flush(seq);
        }
        let n_embd = self.n_embd;
        let lane = &mut self.lanes[seq as usize];
        if lane.off {
            return None;
        }
        let pos0 = if lane.tokens.is_empty() { pos } else { lane.pos0 };
        lane.tokens.push(token);
        lane.hidden.extend_from_slice(&lane.last);
        let decoded = self.context.decode_rows(&lane.tokens, &lane.hidden, n_embd, pos0, seq, true);
        lane.tokens.clear();
        lane.hidden.clear();
        if let Err(error) = decoded {
            tracing::warn!(%error, "the draft context failed; this conversation goes on without drafts");
            lane.off = true;
            return None;
        }
        let guess = self
            .context
            .logits()
            .iter()
            .enumerate()
            .filter(|(_, logit)| logit.is_finite())
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(index, _)| index as u32);
        self.context.seq_rm(seq, pos as i32, -1);
        guess
    }

    /// The model removed its last position on `seq` (a rejected guess): forget its row.
    fn unread_last(&mut self, seq: u32) {
        let n_embd = self.n_embd;
        let lane = &mut self.lanes[seq as usize];
        if lane.off || lane.tokens.is_empty() {
            lane.off = true;
            return;
        }
        lane.tokens.pop();
        let at = lane.hidden.len() - n_embd;
        // The removed row carried the hidden state at the position now last.
        lane.last.copy_from_slice(&lane.hidden[at..]);
        lane.hidden.truncate(at);
    }

    /// The model went back to `len` positions on `seq`, where its hidden state was `last`.
    fn rewind(&mut self, seq: u32, len: usize, last: Option<&[f32]>) {
        let lane = &mut self.lanes[seq as usize];
        let keep = (len as u32).saturating_sub(lane.pos0) as usize;
        if !lane.tokens.is_empty() && keep < lane.tokens.len() {
            lane.tokens.truncate(keep);
            lane.hidden.truncate(keep * self.n_embd);
        }
        self.context.seq_rm(seq, len as i32, -1);
        match last {
            Some(last) if last.len() == self.n_embd => lane.last.copy_from_slice(last),
            _ => lane.off = true,
        }
    }

    /// `to` starts as a copy of `from`'s first `len` rows (a shared leading block), `last` being
    /// the model's hidden state at the last of them. False: `from` has no complete rows (its
    /// drafts were off), and `to` drafts nothing until it starts over.
    fn copy(&mut self, from: u32, to: u32, len: usize, last: &[f32]) -> bool {
        self.flush(from);
        self.clear(to);
        if self.lanes[from as usize].off || last.len() != self.n_embd {
            self.lanes[to as usize].off = true;
            return false;
        }
        self.context.seq_cp(from, to);
        self.context.seq_rm(to, len as i32, -1);
        self.lanes[to as usize].last.copy_from_slice(last);
        true
    }

    /// The sequence starts over (empty).
    fn clear(&mut self, seq: u32) {
        self.context.seq_rm(seq, -1, -1);
        let lane = &mut self.lanes[seq as usize];
        lane.tokens.clear();
        lane.hidden.clear();
        lane.last.fill(0.0);
        lane.off = false;
    }

    fn last(&self, seq: u32) -> Vec<f32> {
        self.lanes[seq as usize].last.clone()
    }
}

// ---- the worker ---------------------------------------------------------------------------------

/// A copy of a sequence's recurrent state when it held `len` tokens.
struct Checkpoint {
    len: usize,
    state: Vec<u8>,
    /// The model's hidden state at the last of those tokens (what the next draft row pairs with).
    hidden: Vec<f32>,
    last_used: Instant,
}

/// An image a sequence holds (or is reading): `len` rows from `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SlotImage {
    start: usize,
    len: usize,
    n_pos: u32,
    key: u64,
}

/// The position after the first `len` rows of a sequence holding `images`: every whole image
/// before it advances the positions by its `n_pos`, not by its rows.
fn position(images: &[SlotImage], len: usize) -> u32 {
    let saved: usize = images.iter().filter(|image| image.start + image.len <= len).map(|image| image.len - image.n_pos as usize).sum();
    (len - saved) as u32
}

/// A leading system block shared across conversations, on a sequence of its own (after the
/// resident conversations' sequences). Empty `tokens`: the sequence holds nothing.
struct Seed {
    seq: u32,
    tokens: Vec<u32>,
    /// The model's hidden state at the block's last token (for the drafter).
    hidden: Vec<f32>,
    /// The draft context holds the block's rows (a conversation started from it may draft).
    drafts: bool,
    last_used: Instant,
}

/// One resident conversation: a sequence of the context.
struct Slot {
    seq: u32,
    key: Option<String>,
    /// The tokens the sequence holds.
    tokens: Vec<u32>,
    /// The images among them.
    images: Vec<SlotImage>,
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
    drafted: u32,
    accepted: u32,
    reporter: crate::progress::Reporter,
    /// Per image of the request: its encoding, once the vision thread sent it (images already in
    /// the sequence are never asked for).
    encoded: Vec<Option<Result<Arc<Encoded>, String>>>,
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
    drafter: Option<Drafter>,
    n_embd: usize,
    /// Hidden-state rows of the last batch the model read (for the drafter).
    hidden: Vec<f32>,
    compiler: GrammarCompiler,
    rx: mpsc::Receiver<Command>,
    slots: Vec<Slot>,
    seeds: Vec<Seed>,
    /// Where seeds are saved (None: memory only).
    store: Option<crate::seeds::Store>,
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
    vision: Option<Arc<VisionHandle>>,
    /// The engine's own channel, for the vision thread's encodings.
    encoded_tx: mpsc::Sender<Command>,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        context: Context,
        drafter: Option<Drafter>,
        compiler: GrammarCompiler,
        loaded: &Loaded,
        rx: mpsc::Receiver<Command>,
        view: Arc<Mutex<View>>,
        publisher: Publisher,
        vision: Option<Arc<VisionHandle>>,
        encoded_tx: mpsc::Sender<Command>,
        store: Option<crate::seeds::Store>,
    ) -> Self {
        let n_embd = drafter.as_ref().map_or(0, |drafter| drafter.n_embd);
        let slots = (0..loaded.resident)
            .map(|seq| Slot { seq: seq as u32, key: None, tokens: Vec::new(), images: Vec::new(), checkpoints: Vec::new(), busy: None, last_used: Instant::now() })
            .collect();
        // The seeds' sequences follow every resident sequence the plan made (an operator cap on
        // resident conversations leaves some of those unused).
        let seeds = (loaded.plan.resident..loaded.plan.sequences)
            .map(|seq| Seed { seq: seq as u32, tokens: Vec::new(), hidden: Vec::new(), drafts: false, last_used: Instant::now() })
            .collect();
        let worker = Self {
            context,
            drafter,
            n_embd,
            hidden: Vec::new(),
            compiler,
            rx,
            slots,
            seeds,
            store,
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
            vision,
            encoded_tx,
        };
        worker.publish_view();
        worker
    }

    fn run(&mut self) {
        let mut stalled = false;
        // A quiet deadline wakes admission once; a request that still cannot start then waits
        // for a state-changing command instead of polling the expired deadline.
        let mut idle_room_checked = None;
        loop {
            // Block for a command only when nothing can step (requests waiting for an image's
            // encoding wake the worker with it) and nothing waiting can start.
            let idle = !self.any_runnable() && (self.waiting.is_empty() || stalled);
            // An idle-only generation waiting for the quiet period wakes the worker when it ends.
            let wake = self.waiting.iter().any(|request| idle_generation(request)).then(|| self.last_work + IDLE_ROOM_AFTER).filter(|at| Some(*at) != idle_room_checked);
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
            let admission_at = Instant::now();
            self.admit();
            let quiet_at = self.last_work + IDLE_ROOM_AFTER;
            if admission_at >= quiet_at {
                idle_room_checked = Some(quiet_at);
            }
            stalled = !self.any_runnable() && !self.waiting.is_empty();
            if self.any_runnable() {
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
            Command::Encoded { request, image, result } => {
                if let Some(active) = self.active.iter_mut().find(|active| active.request.id == request)
                    && let Some(slot) = active.encoded.get_mut(image)
                {
                    *slot = Some(result);
                }
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
        self.slots.iter().map(|slot| slot.tokens.len() as u64).sum::<u64>() + self.seed_tokens() + self.reserved_tokens()
    }

    /// Cells only the seeds hold: a seed's rows are a conversation's too while it starts with them.
    fn seed_tokens(&self) -> u64 {
        self.seeds
            .iter()
            .filter(|seed| !self.slots.iter().any(|slot| slot.tokens.starts_with(&seed.tokens)))
            .map(|seed| seed.tokens.len() as u64)
            .sum()
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
        // A prewarm of a leading block for conversations not created yet: done without a slot when
        // the block is kept (a seed, an idle conversation that starts with it, or a saved copy);
        // else it is read like any prewarm and kept at its end. Without a block it ends at once.
        if self.waiting[position].shared_only {
            let block = self.waiting[position].shared.filter(|&shared| shared > 0 && shared < self.waiting[position].prompt.len());
            let kept = match block {
                Some(shared) => self.seed_holding(position, shared),
                None => Some(0),
            };
            if let Some(cached) = kept {
                let Some(request) = self.waiting.remove(position) else { return Placement::Wait };
                self.publish_ended(&request, None, "prefilled");
                let _ = request.events.send(Event::Done(Outcome {
                    finish: Finish::Prefilled,
                    prompt_tokens: request.prompt.len() as u32,
                    cached_tokens: cached as u32,
                    completion_tokens: 0,
                    queue_ms: ms(request.arrival),
                    prefill_ms: 0.0,
                    decode_ms: 0.0,
                    slot: 0,
                    draft_total: 0,
                    draft_accepted: 0,
                }));
                return Placement::Admitted;
            }
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
                None if self.drop_seed() => {}
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

    /// Queued idle-only generations yield in admission order; running generations still block.
    fn serving_other(&self, key: &str) -> bool {
        let real = |request: &Request| request.key != key && !matches!(request.work, Work::Prewarm);
        self.active.iter().any(|active| real(&active.request)) || self.waiting.iter().any(|request| real(request) && !idle_generation(request))
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
        if let Some(drafter) = self.drafter.as_mut() {
            drafter.clear(seq);
        }
        let slot = &mut self.slots[index];
        slot.key = None;
        slot.tokens.clear();
        slot.images.clear();
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
        let mut common = slot.tokens.iter().zip(prompt.iter()).take_while(|(a, b)| a == b).count();
        // Image rows match only as the same whole image (every row is the same pad token).
        for image in request.images.iter().take_while(|image| image.start < common) {
            let same = slot.images.iter().any(|held| held.start == image.start && held.len == image.rows && held.key == image.key);
            if !same || image.start + image.rows > common {
                common = image.start;
                break;
            }
        }
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
            let restored = best.and_then(|index| {
                let slot = &self.slots[slot_index];
                let checkpoint = &slot.checkpoints[index];
                let len = checkpoint.len;
                let at = position(&slot.images, len);
                (self.context.set_state(seq, true, &checkpoint.state) && self.context.seq_rm(seq, at as i32, -1)).then_some((index, len))
            });
            match restored {
                Some((position, len)) => {
                    if let Some(drafter) = self.drafter.as_mut() {
                        drafter.rewind(seq, len, Some(&self.slots[slot_index].checkpoints[position].hidden));
                    }
                    let slot = &mut self.slots[slot_index];
                    slot.checkpoints[position].last_used = Instant::now();
                    // The rows after the rollback point will be rewritten: longer checkpoints are void.
                    slot.checkpoints.retain(|checkpoint| checkpoint.len <= len);
                    slot.tokens.truncate(len);
                    slot.images.retain(|image| image.start + image.len <= len);
                    len
                }
                None => {
                    self.context.seq_rm(seq, -1, -1);
                    if let Some(drafter) = self.drafter.as_mut() {
                        drafter.clear(seq);
                    }
                    let slot = &mut self.slots[slot_index];
                    slot.tokens.clear();
                    slot.images.clear();
                    slot.checkpoints.clear();
                    0
                }
            }
        };
        // A shared leading block that covers more of the prompt than the conversation holds (a new
        // conversation, or one whose history changed before it): start from a copy of it.
        let cached = match request.shared.filter(|&shared| shared > cached && shared < prompt.len()) {
            Some(shared) if !request.images.iter().any(|image| image.start < shared) => match self.seed_for(&prompt[..shared]) {
                Some(seed) => self.start_from_seed(slot_index, seed),
                None => cached,
            },
            _ => cached,
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
        // The images still to read go to the vision thread now; the text before them is read meanwhile.
        let mut encoded: Vec<Option<Result<Arc<Encoded>, String>>> = request.images.iter().map(|_| None).collect();
        for (index, image) in request.images.iter().enumerate().filter(|(_, image)| image.start + image.rows > cached) {
            let Some(vision) = &self.vision else {
                encoded[index] = Some(Err("this model takes no images".into()));
                continue;
            };
            let tx = self.encoded_tx.clone();
            let id = request.id;
            let done = Box::new(move |result| {
                let _ = tx.send(Command::Encoded { request: id, image: index, result });
            });
            if let Err(error) = vision.encode(image.path.clone(), image.sha256.clone(), done) {
                encoded[index] = Some(Err(error));
            }
        }
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
            drafted: 0,
            accepted: 0,
            reporter: crate::progress::Reporter::default(),
            encoded,
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
        let runnable: Vec<usize> = (0..self.active.len()).filter(|&index| self.runnable(index)).collect();
        let Some(best) = runnable.iter().map(|&index| class(self.active[index].request.priority)).min() else { return };
        let candidates: Vec<usize> = runnable.into_iter().filter(|&index| class(self.active[index].request.priority) == best).collect();
        let turn = &mut self.turn[best];
        *turn = (*turn + 1) % candidates.len().max(1);
        let index = candidates[*turn % candidates.len()];
        if let Err(error) = self.advance(index) {
            tracing::warn!(%error, "a request failed in the engine");
            self.fail(index, error);
        }
    }

    /// The image whose rows a prefill at `next` reads, if any.
    fn image_at(active: &Active, next: usize) -> Option<usize> {
        active.request.images.iter().position(|image| image.start <= next && next < image.start + image.rows)
    }

    /// Whether the request can take a step now: not while its next rows are an image the vision
    /// thread has not encoded yet (a stopped request can always end).
    fn runnable(&self, index: usize) -> bool {
        let active = &self.active[index];
        if active.request.stop.load(Ordering::SeqCst) || active.request.events.is_closed() {
            return true;
        }
        match active.phase {
            Phase::Prefill { next } => Self::image_at(active, next).is_none_or(|image| active.encoded[image].is_some()),
            Phase::Generate(_) => true,
        }
    }

    fn any_runnable(&self) -> bool {
        (0..self.active.len()).any(|index| self.runnable(index))
    }

    fn advance(&mut self, index: usize) -> Result<(), String> {
        let slot_index = self.active[index].slot;
        let start = match &self.active[index].phase {
            Phase::Prefill { next } => *next,
            Phase::Generate(_) => return self.decode_step(index),
        };
        // A leading block's prewarm reads the block alone.
        let end = match (self.active[index].request.shared_only, self.active[index].request.shared) {
            (true, Some(shared)) => shared,
            _ => self.active[index].request.prompt.len() - 1,
        };
        if start < end
            && let Some(image) = Self::image_at(&self.active[index], start)
        {
            // An image's rows, at most a prefill step of them.
            let encoded = match &self.active[index].encoded[image] {
                Some(Ok(encoded)) => encoded.clone(),
                Some(Err(error)) => return Err(format!("image {} could not be read: {error}", image + 1)),
                None => return Ok(()),
            };
            let held = self.active[index].request.images[image].clone();
            if encoded.rows != held.rows || encoded.n_pos != held.n_pos {
                return Err(format!("image {} encoded to {} rows where the prompt has {}", image + 1, encoded.rows, held.rows));
            }
            let from = start - held.start;
            let stop = (start + PREFILL_CHUNK).min(held.start + held.rows);
            let rows = self.active[index].request.prompt[start..stop].to_vec();
            self.feed_image(slot_index, &held, &encoded, from, &rows)?;
            if let Phase::Prefill { next } = &mut self.active[index].phase {
                *next = stop;
            }
            // Its embeddings are no longer needed once every row is in.
            if stop == held.start + held.rows {
                self.active[index].encoded[image] = Some(Err("already read".into()));
            }
            self.report(index, progress_phase::PREFILL);
            return Ok(());
        }
        if start < end {
            // Up to the next reuse point (checkpointed when reached) or the next image.
            let reuse = self.active[index].request.checkpoints.iter().copied().filter(|&point| point > start && point < end).min();
            let image = self.active[index].request.images.iter().map(|image| image.start).filter(|&at| at > start).min();
            let stop = (start + PREFILL_CHUNK).min(end).min(reuse.unwrap_or(usize::MAX)).min(image.unwrap_or(usize::MAX));
            let chunk = self.active[index].request.prompt[start..stop].to_vec();
            self.feed(slot_index, &chunk, Logits::None)?;
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
        // Everything but the last token is in the sequence: checkpoint it, then generate.
        self.checkpoint(slot_index, end);
        self.seed(index, end);
        if matches!(self.active[index].request.work, Work::Prewarm) {
            self.finish(index, Finish::Prefilled);
            return Ok(());
        }
        let last = self.active[index].request.prompt[end];
        self.feed(slot_index, &[last], Logits::Last)?;
        let Work::Generate { sampling, grammar, grammar_trigger, .. } = &self.active[index].request.work else { return Ok(()) };
        let grammar = match grammar {
            Some(spec) => Some(self.compiler.compile(spec, grammar_trigger.clone()).map_err(|error| format!("the grammar does not compile: {error}"))?),
            None => None,
        };
        let sampler = Sampler::new(sampling.clone(), &self.active[index].request.prompt);
        let mut generation = Box::new(Generation { sampler, grammar, pending: 0 });
        generation.pending = self.sample(&mut generation, 0)?;
        self.active[index].phase = Phase::Generate(generation);
        Ok(())
    }

    /// Feed `tokens` to the slot's sequence after what it holds (one batch), and hand the model's
    /// hidden states to the drafter.
    fn feed(&mut self, slot_index: usize, tokens: &[u32], logits: Logits) -> Result<(), String> {
        let seq = self.slots[slot_index].seq;
        let slot = &self.slots[slot_index];
        let pos = position(&slot.images, slot.tokens.len());
        self.context.decode_batch(tokens, pos, seq, logits)?;
        self.note_read(slot_index, tokens);
        Ok(())
    }

    /// Feed rows `from..` of an image (`rows` are their pad tokens in the prompt).
    fn feed_image(&mut self, slot_index: usize, image: &PromptImage, encoded: &Encoded, from: usize, rows: &[u32]) -> Result<(), String> {
        let slot = &mut self.slots[slot_index];
        if from == 0 {
            slot.images.push(SlotImage { start: image.start, len: image.rows, n_pos: image.n_pos, key: image.key });
        }
        let seq = slot.seq;
        let first = position(&slot.images, image.start) as i32;
        // Absolute positions: the image's own (t, x, y) from its first position; M-RoPE leaves z at 0.
        let positions: Vec<i32> = encoded.positions[from * 4..(from + rows.len()) * 4]
            .chunks_exact(4)
            .flat_map(|row| [row[0] + first, row[1] + first, row[2] + first, if encoded.mrope { row[3] } else { row[3] + first }])
            .collect();
        let embd = &encoded.embd[from * encoded.n_embd..(from + rows.len()) * encoded.n_embd];
        self.context.decode_image(embd, &positions, encoded.n_embd, 0, rows.len(), encoded.mrope, seq)?;
        self.note_read(slot_index, rows);
        Ok(())
    }

    /// The model read `tokens` after what the slot held: keep them, and hand the model's hidden
    /// states to the drafter (whose own positions count rows).
    fn note_read(&mut self, slot_index: usize, tokens: &[u32]) {
        let seq = self.slots[slot_index].seq;
        let pos0 = self.slots[slot_index].tokens.len() as u32;
        self.slots[slot_index].tokens.extend_from_slice(tokens);
        if let Some(drafter) = self.drafter.as_mut() {
            self.hidden.clear();
            for row in 0..tokens.len() {
                let hidden = self.context.hidden_ith(row, self.n_embd);
                if hidden.len() != self.n_embd {
                    self.hidden.clear();
                    break;
                }
                self.hidden.extend_from_slice(hidden);
            }
            if self.hidden.len() == tokens.len() * self.n_embd {
                drafter.note(seq, tokens, pos0, &self.hidden);
            } else {
                drafter.lanes[seq as usize].off = true;
            }
        }
    }

    /// Remove the slot's last token (a rejected guess) from the model and the drafter.
    fn unread_last(&mut self, slot_index: usize) -> Result<(), String> {
        let seq = self.slots[slot_index].seq;
        let slot = &self.slots[slot_index];
        let pos = position(&slot.images, slot.tokens.len().saturating_sub(1));
        if !self.context.seq_rm(seq, pos as i32, -1) {
            return Err("the model could not remove a rejected draft".into());
        }
        self.slots[slot_index].tokens.pop();
        if let Some(drafter) = self.drafter.as_mut() {
            drafter.unread_last(seq);
        }
        Ok(())
    }

    /// Sample the next token from row `row` of the last batch's logits and note it with the
    /// grammar.
    fn sample(&mut self, generation: &mut Generation, row: usize) -> Result<u32, String> {
        self.logits.clear();
        self.logits.extend_from_slice(self.context.logits_ith(row));
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

    /// Hand out the pending token, feed it (with a drafted guess of the token after it, when the
    /// request goes on), sample the next. A guess the model confirms is handed out in the same step.
    fn decode_step(&mut self, index: usize) -> Result<(), String> {
        let slot_index = self.active[index].slot;
        let seq = self.slots[slot_index].seq;
        // An accepted guess: already read by the model, its logits in this row of the last batch.
        let mut read_row: Option<usize> = None;
        loop {
            let Phase::Generate(generation) = &self.active[index].phase else { return Ok(()) };
            let token = generation.pending;
            let now = Instant::now();
            let active = &mut self.active[index];
            active.first_token.get_or_insert(now);
            active.last_token = Some(now);
            if self.stop_tokens.contains(&token) {
                // An accepted guess that ends the answer leaves the sequence as plain decoding does.
                if read_row.is_some() {
                    self.unread_last(slot_index)?;
                }
                self.finish(index, Finish::Stop);
                return Ok(());
            }
            let active = &mut self.active[index];
            active.generated += 1;
            let _ = active.request.events.send(Event::Token(token));
            active.emitted.push(token);
            let max_tokens = match &active.request.work {
                Work::Generate { max_tokens, .. } => *max_tokens,
                Work::Prewarm => None,
            };
            let at_max = max_tokens.is_some_and(|max| active.generated >= max);
            let at_budget = match (active.thinking, &active.request.work) {
                (Some(used), Work::Generate { think_budget: Some(budget), .. }) => !active.emitted.ends_with(&budget.end) && used + 1 >= budget.tokens,
                _ => false,
            };
            let (row, guess) = match read_row {
                Some(row) => (row, None),
                None => {
                    // Guess only when the request goes on after this token.
                    let pos = self.slots[slot_index].tokens.len() as u32;
                    let guess = if at_max || at_budget { None } else { self.drafter.as_mut().and_then(|drafter| drafter.draft(seq, token, pos)) };
                    match guess {
                        Some(guess) => self.feed(slot_index, &[token, guess], Logits::All)?,
                        None => self.feed(slot_index, &[token], Logits::Last)?,
                    }
                    (0, guess)
                }
            };
            self.report(index, progress_phase::DECODE);
            if at_max {
                self.finish(index, Finish::Length);
                return Ok(());
            }
            // The reasoning budget.
            let active = &mut self.active[index];
            if let (Some(used), Work::Generate { think_budget: Some(budget), .. }) = (active.thinking, &active.request.work) {
                if active.emitted.ends_with(&budget.end) {
                    active.thinking = None;
                } else if at_budget {
                    return self.close_thinking(index);
                } else {
                    active.thinking = Some(used + 1);
                }
            }
            let Phase::Generate(mut generation) = std::mem::replace(&mut self.active[index].phase, Phase::Prefill { next: 0 }) else { return Ok(()) };
            generation.sampler.push(token);
            let next = self.sample(&mut generation, row);
            self.active[index].phase = Phase::Generate(generation);
            let next = next?;
            if let Phase::Generate(generation) = &mut self.active[index].phase {
                generation.pending = next;
            }
            let Some(guess) = guess else { return Ok(()) };
            self.active[index].drafted += 1;
            if next != guess {
                return self.unread_last(slot_index);
            }
            self.active[index].accepted += 1;
            read_row = Some(1);
        }
    }

    /// The reasoning budget is reached: feed the closing tokens (handed out like generated ones, so
    /// the output parser sees reasoning end) and go on with the answer on the same sequence.
    fn close_thinking(&mut self, index: usize) -> Result<(), String> {
        let Work::Generate { think_budget: Some(budget), .. } = &self.active[index].request.work else { return Ok(()) };
        let close = budget.close.clone();
        let slot_index = self.active[index].slot;
        tracing::info!(reasoning_tokens = self.active[index].thinking.unwrap_or(0) + 1, "reasoning budget reached; ending the reasoning block");
        let active = &mut self.active[index];
        active.thinking = None;
        for token in &close {
            let _ = active.request.events.send(Event::Token(*token));
        }
        active.emitted.extend_from_slice(&close);
        active.generated += close.len() as u32;
        self.feed(slot_index, &close, Logits::Last)?;
        let Phase::Generate(mut generation) = std::mem::replace(&mut self.active[index].phase, Phase::Prefill { next: 0 }) else { return Ok(()) };
        for token in &close {
            generation.sampler.push(*token);
        }
        // The answer starts now: a grammar waiting for the end of reasoning applies from here.
        if let Some(grammar) = generation.grammar.as_mut() {
            grammar.engage_now();
        }
        let next = self.sample(&mut generation, close.len().saturating_sub(1));
        self.active[index].phase = Phase::Generate(generation);
        if let Phase::Generate(generation) = &mut self.active[index].phase {
            generation.pending = next?;
        }
        Ok(())
    }

    /// Keep a checkpoint of the slot's sequence at `len` tokens (when it holds exactly that many
    /// and none is kept there yet), evicting the least recently used beyond the fitted count. The
    /// shortest stays (the end of the leading system block): a later turn's prompt differs after
    /// the history, so it is the point every next call can resume from.
    fn checkpoint(&mut self, slot_index: usize, len: usize) {
        let slot = &self.slots[slot_index];
        if self.checkpoints == 0 || len == 0 || slot.tokens.len() != len || slot.checkpoints.iter().any(|checkpoint| checkpoint.len == len) {
            return;
        }
        let seq = slot.seq;
        let state = self.context.state(seq, true);
        if state.is_empty() {
            return;
        }
        let hidden = self.drafter.as_ref().map(|drafter| drafter.last(seq)).unwrap_or_default();
        let checkpoints = &mut self.slots[slot_index].checkpoints;
        checkpoints.push(Checkpoint { len, state, hidden, last_used: Instant::now() });
        while checkpoints.len() > self.checkpoints {
            let first = checkpoints.iter().map(|checkpoint| checkpoint.len).min();
            let Some(oldest) = checkpoints
                .iter()
                .enumerate()
                .filter(|(_, checkpoint)| checkpoints.len() == 1 || Some(checkpoint.len) != first)
                .min_by_key(|(_, checkpoint)| checkpoint.last_used)
                .map(|(position, _)| position)
            else {
                break;
            };
            checkpoints.remove(oldest);
        }
    }

    // ---- shared leading blocks ----------------------------------------------------------------

    /// For waiting `shared_only` request `position`, whose block ends at `shared`: the block's
    /// length when it is kept now (a seed, a saved copy, or an idle conversation that starts with
    /// it and has its checkpoint). None: the block has to be read.
    fn seed_holding(&mut self, position: usize, shared: usize) -> Option<usize> {
        let block = self.waiting[position].prompt[..shared].to_vec();
        if self.seed_for(&block).is_some() {
            return Some(shared);
        }
        let slot = self.slots.iter().position(|slot| {
            slot.busy.is_none()
                && slot.tokens.starts_with(&block)
                && slot.images.iter().all(|image| image.start >= shared)
                && slot.checkpoints.iter().any(|checkpoint| checkpoint.len == shared)
        })?;
        self.keep_seed(slot, shared).then_some(shared)
    }

    /// The seed that is exactly `block`: kept, or restored from disk.
    fn seed_for(&mut self, block: &[u32]) -> Option<usize> {
        if let Some(index) = self.seeds.iter().position(|seed| seed.tokens == block) {
            self.seeds[index].last_used = Instant::now();
            return Some(index);
        }
        self.restore_seed(block)
    }

    /// Keep active request `index`'s leading block as a seed when its slot holds exactly that
    /// (`length` is its `shared` point) and no seed holds the same tokens.
    fn seed(&mut self, index: usize, length: usize) {
        let request = &self.active[index].request;
        let slot_index = self.active[index].slot;
        if request.shared != Some(length) || request.images.iter().any(|image| image.start < length) || self.slots[slot_index].tokens.len() != length {
            return;
        }
        if let Some(seed) = self.seeds.iter_mut().find(|seed| seed.tokens[..] == request.prompt[..length]) {
            seed.last_used = Instant::now();
            return;
        }
        self.keep_seed(slot_index, length);
    }

    /// The seed sequence a new block takes: an empty one, else the least recently used.
    fn seed_place(&self) -> Option<usize> {
        self.seeds
            .iter()
            .position(|seed| seed.tokens.is_empty())
            .or_else(|| self.seeds.iter().enumerate().min_by_key(|(_, seed)| seed.last_used).map(|(index, _)| index))
    }

    /// Keep the first `len` tokens of slot `slot_index`'s sequence as a seed. The slot holds them
    /// and a checkpoint at `len`; the rows are shared with the slot, not copied, and a slot already
    /// past the block puts the seed back to the checkpoint.
    fn keep_seed(&mut self, slot_index: usize, len: usize) -> bool {
        let Some(place) = self.seed_place() else { return false };
        let Some(checkpoint) = self.slots[slot_index].checkpoints.iter().position(|checkpoint| checkpoint.len == len) else { return false };
        self.clear_seed(place);
        let seq = self.seeds[place].seq;
        let slot = &self.slots[slot_index];
        let (from, longer) = (slot.seq, slot.tokens.len() > len);
        self.context.seq_cp(from, seq);
        if longer && !(self.context.set_state(seq, true, &slot.checkpoints[checkpoint].state) && self.context.seq_rm(seq, len as i32, -1)) {
            self.context.seq_rm(seq, -1, -1);
            return false;
        }
        let hidden = slot.checkpoints[checkpoint].hidden.clone();
        let tokens = slot.tokens[..len].to_vec();
        let drafts = self.drafter.as_mut().is_some_and(|drafter| drafter.copy(from, seq, len, &hidden));
        self.seeds[place] = Seed { seq, tokens, hidden, drafts, last_used: Instant::now() };
        tracing::info!(length = len, "keeping a shared leading block");
        self.save_seed(place);
        true
    }

    /// Put seed `index`'s block on slot `slot_index` in place of what the slot holds, with its
    /// checkpoint at the block's end; returns the block's length.
    fn start_from_seed(&mut self, slot_index: usize, index: usize) -> usize {
        let seq = self.slots[slot_index].seq;
        let seed = &self.seeds[index];
        let (from, len) = (seed.seq, seed.tokens.len());
        self.context.seq_rm(seq, -1, -1);
        self.context.seq_cp(from, seq);
        if let Some(drafter) = self.drafter.as_mut()
            && !(seed.drafts && drafter.copy(from, seq, len, &seed.hidden))
        {
            drafter.clear(seq);
            drafter.lanes[seq as usize].off = true;
        }
        // The recurrent state is the seed's until the slot writes (llama.cpp copies it then).
        let checkpoint = Checkpoint { len, state: self.context.state(seq, true), hidden: seed.hidden.clone(), last_used: Instant::now() };
        let tokens = seed.tokens.clone();
        self.seeds[index].last_used = Instant::now();
        let slot = &mut self.slots[slot_index];
        slot.tokens = tokens;
        slot.images.clear();
        slot.checkpoints = if checkpoint.state.is_empty() { Vec::new() } else { vec![checkpoint] };
        tracing::debug!(cache_key = ?slot.key, tokens = len, "started from a shared leading block");
        len
    }

    fn clear_seed(&mut self, index: usize) {
        let seq = self.seeds[index].seq;
        self.context.seq_rm(seq, -1, -1);
        if let Some(drafter) = self.drafter.as_mut() {
            drafter.clear(seq);
        }
        let seed = &mut self.seeds[index];
        seed.tokens.clear();
        seed.hidden = Vec::new();
        seed.drafts = false;
    }

    /// Drop the least recently used seed whose rows no conversation shares (room in the pool).
    /// Its saved copy stays on disk.
    fn drop_seed(&mut self) -> bool {
        let Some(index) = self
            .seeds
            .iter()
            .enumerate()
            .filter(|(_, seed)| !seed.tokens.is_empty() && !self.slots.iter().any(|slot| slot.tokens.starts_with(&seed.tokens)))
            .min_by_key(|(_, seed)| seed.last_used)
            .map(|(index, _)| index)
        else {
            return false;
        };
        tracing::info!(tokens = self.seeds[index].tokens.len(), "dropped a shared leading block");
        self.clear_seed(index);
        true
    }

    /// Save seed `index` to disk (written on another thread).
    fn save_seed(&mut self, index: usize) {
        let Some(store) = &self.store else { return };
        let seed = &self.seeds[index];
        if store.has(&seed.tokens) {
            return;
        }
        let state = self.context.state(seed.seq, false);
        if state.is_empty() {
            return;
        }
        let draft_state = match self.drafter.as_mut() {
            Some(drafter) if seed.drafts => drafter.context.state(seed.seq, false),
            _ => Vec::new(),
        };
        store.save(crate::seeds::Saved { tokens: seed.tokens.clone(), state, hidden: seed.hidden.clone(), draft_state });
    }

    /// Restore the saved block that is exactly `block` onto a seed sequence (the least recently
    /// used one makes room). None: none is saved, the pool has no room, or it does not restore.
    fn restore_seed(&mut self, block: &[u32]) -> Option<usize> {
        let place = self.seed_place()?;
        let freed = if self.slots.iter().any(|slot| slot.tokens.starts_with(&self.seeds[place].tokens)) { 0 } else { self.seeds[place].tokens.len() as u64 };
        if self.pool_used().saturating_sub(freed) + block.len() as u64 > self.pool_tokens {
            return None;
        }
        let started = Instant::now();
        let saved = self.store.as_ref()?.load(block)?;
        self.clear_seed(place);
        let seq = self.seeds[place].seq;
        // Deleted when it does not restore (it is saved again once the block is read), so a
        // request waiting in admission never reads the same file twice.
        if !self.context.set_state(seq, false, &saved.state) || self.context.seq_pos_max(seq) != block.len() as i32 - 1 {
            self.context.seq_rm(seq, -1, -1);
            if let Some(store) = &self.store {
                store.forget(block);
            }
            tracing::info!(tokens = block.len(), "a saved shared leading block did not restore; reading it instead");
            return None;
        }
        let drafts = match self.drafter.as_mut() {
            Some(drafter) if !saved.draft_state.is_empty() && saved.hidden.len() == drafter.n_embd => {
                let restored = drafter.context.set_state(seq, false, &saved.draft_state);
                if !restored {
                    drafter.clear(seq);
                }
                restored
            }
            _ => false,
        };
        self.seeds[place] = Seed { seq, tokens: block.to_vec(), hidden: saved.hidden, drafts, last_used: Instant::now() };
        tracing::info!(tokens = block.len(), ms = started.elapsed().as_millis() as u64, "restored a shared leading block from disk");
        Some(place)
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
        // An idle-only generation leaves nothing in the pool, like a leading block's prewarm (the
        // block lives on as a seed; no conversation has this key).
        if active.request.shared_only || (active.request.idle_only && !matches!(active.request.work, Work::Prewarm)) {
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
            draft_total: active.drafted,
            draft_accepted: active.accepted,
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
