//! `model@1`: local inference (llama.cpp on Metal by default).
//!
//! The model plugin owns weights, slots, the shared KV pool, prompt caching, speculative decoding
//! and scheduling by priority. Callers never manage slots: they pass a `cache_key` (session or agent
//! id) so the plugin can keep that conversation's prefix warm, and a `priority`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod methods {
    pub const INFO: &str = "model.info";
    /// Chat completion. Streams [`super::ChatDelta`] items, ends with [`super::ChatResult`].
    pub const CHAT: &str = "model.chat";
    /// Raw prompt completion (no chat template). Streams deltas, ends with [`super::ChatResult`].
    pub const COMPLETE: &str = "model.complete";
    pub const TOKENIZE: &str = "model.tokenize";
    /// Count tokens of a chat request after templating (for context accounting).
    pub const COUNT: &str = "model.count";
    pub const SLOTS: &str = "model.slots";
    /// Persist a cache key's KV state to disk and free its slot.
    pub const HIBERNATE: &str = "model.hibernate";
    /// Keep a cache key resident while its turn runs (`hold: true`), or let it go (`hold: false`):
    /// a held conversation is not spilled for being idle (a turn waiting on a long tool is not
    /// idle); it can still be spilled when the pool needs the room.
    pub const HOLD: &str = "model.hold";
    /// Warm a cache key's prefix (prefill without generating), e.g. the shared system block.
    pub const PREWARM: &str = "model.prewarm";
    /// Available model variants and the active one (e.g. stock vs. uncensored).
    pub const VARIANTS: &str = "model.variants";
    /// Switch the active variant (reloads weights).
    pub const SELECT: &str = "model.select";
}

pub mod topics {
    /// Progress of one request ([`super::Progress`]): published when it is queued, when it starts,
    /// as its prefill and its decoding advance (at most [`super::PROGRESS_EVERY_MS`] apart per
    /// request; a phase change is always published) and when it ends. Keyed by `cache_key`, so a
    /// conversation's view follows its own requests.
    pub const PROGRESS: &str = "model.progress";
}

/// Least interval between two progress events of one request in the same phase (milliseconds).
pub const PROGRESS_EVERY_MS: u64 = 250;

/// Phases of a request in [`Progress`].
pub mod progress_phase {
    /// Waiting for a slot (or for its conversation's running request).
    pub const QUEUED: &str = "queued";
    /// Reading the prompt: `prefill_done` of `prefill_total` new tokens.
    pub const PREFILL: &str = "prefill";
    /// Writing: `generated` tokens at `tok_s`.
    pub const DECODE: &str = "decode";
    /// Finished (`finish` says how).
    pub const DONE: &str = "done";
}

/// One `model.progress` event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Progress {
    /// Unique per request within the model plugin's process.
    pub request: String,
    pub cache_key: String,
    pub priority: Priority,
    /// `generate` or `prewarm`.
    pub work: String,
    /// [`progress_phase`].
    pub phase: String,
    pub prompt_tokens: u32,
    /// Prompt tokens served from cache (known once started).
    #[serde(default)]
    pub cached_tokens: u32,
    /// Prompt tokens read so far of `prefill_total` (the prompt less what was cached).
    #[serde(default)]
    pub prefill_done: u32,
    #[serde(default)]
    pub prefill_total: u32,
    #[serde(default)]
    pub generated: u32,
    /// Decoding rate so far (generated tokens per second since the first one).
    #[serde(default)]
    pub tok_s: f64,
    /// Requests ahead of it in the queue (phase `queued`).
    #[serde(default)]
    pub ahead: u32,
    /// How it ended (phase `done`): stop | length | stopped | prefilled | failed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub finish: String,
    #[serde(default)]
    pub elapsed_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub model_id: String,
    pub variant: String,
    pub path: String,
    pub family: String,
    pub quant: String,
    pub file_bytes: u64,
    /// Tokens of the shared pool. It is not divided among conversations: any one may use all of
    /// it, up to `ctx_native`.
    pub ctx_total: u32,
    /// The model's native context length.
    pub ctx_native: u32,
    /// Conversations the pool can hold resident at once (each one's fixed state, no context): an
    /// upper bound for concurrency, not a share of the pool. Conversations holding context leave
    /// room for fewer; the model spills idle ones to disk or queues requests when it is full.
    pub slots: u32,
    pub kv_type: String,
    /// Speculative decoding in use, e.g. `draft-mtp` (empty when none).
    pub speculative: String,
    pub backend: String,
    /// Whether the Metal tensor (M5 neural accelerator) path is active, when detectable.
    #[serde(default)]
    pub tensor_api: Option<bool>,
    pub engine_revision: String,
    #[serde(default)]
    pub extra: Value,
    /// Image input of the loaded model (absent: text only). Typed form of what
    /// `extra.input_modalities` advertises; see [`ModelInfo::accepts_images`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<VisionInfo>,
}

/// Image understanding of a model (`ModelInfo.vision`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct VisionInfo {
    /// Image content parts are accepted now (the model's vision encoder is installed and enabled).
    pub available: bool,
    /// The vision encoder is resident in memory right now. It is loaded when a request contains an
    /// image and unloaded after `idle_unload_s` seconds without one.
    #[serde(default)]
    pub loaded: bool,
    /// Pixel budget per image: larger images are downscaled to it (aspect ratio kept).
    #[serde(default)]
    pub max_image_pixels: u64,
    /// Prompt tokens one image of `max_image_pixels` takes at most.
    #[serde(default)]
    pub max_image_tokens: u32,
    #[serde(default)]
    pub idle_unload_s: u64,
    /// Why images are not available (e.g. the encoder is not installed), when they are not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    /// system | user | assistant | tool
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The message as content parts (text and images), sent only to a model whose `model.info`
    /// advertises image input ([`ModelInfo::accepts_images`]). When present it takes precedence
    /// over `content`, which stays the plain text, so text-only consumers keep working.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ContentPart>,
}

/// One part of a multimodal message. An image is referenced by its file in the attachment store
/// (never inlined): the model plugin reads the file and may cache its encoding by `sha256`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        sha256: String,
        path: String,
        mime: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
    },
}

/// Input modalities a model can advertise in `ModelInfo.extra.input_modalities`.
pub mod modalities {
    pub const TEXT: &str = "text";
    pub const IMAGE: &str = "image";
}

impl ModelInfo {
    /// Whether the model takes image content parts: the typed [`ModelInfo::vision`] says so, or
    /// (for plugins that only fill `extra`) `extra.input_modalities` lists `image`. Absent means
    /// text only.
    pub fn accepts_images(&self) -> bool {
        if let Some(vision) = &self.vision {
            return vision.available;
        }
        self.extra
            .get("input_modalities")
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(modalities::IMAGE)))
    }
}

impl ChatMessage {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_owned(),
            content: content.into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            parts: Vec::new(),
        }
    }
}

/// A function the model may call (JSON Schema parameters).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The raw JSON argument text the model produced (may be malformed; repaired by the tools plugin).
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sampling {
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_top_p")]
    pub top_p: f32,
    #[serde(default = "default_top_k")]
    pub top_k: u32,
    #[serde(default)]
    pub min_p: f32,
    #[serde(default)]
    pub presence_penalty: f32,
    #[serde(default)]
    pub repeat_penalty: Option<f32>,
    #[serde(default)]
    pub seed: Option<u64>,
}

fn default_temperature() -> f32 {
    0.7
}
fn default_top_p() -> f32 {
    0.8
}
fn default_top_k() -> u32 {
    20
}

impl Default for Sampling {
    fn default() -> Self {
        Self {
            temperature: default_temperature(),
            top_p: default_top_p(),
            top_k: default_top_k(),
            min_p: 0.0,
            presence_penalty: 0.0,
            repeat_penalty: None,
            seed: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Thinking {
    pub enabled: bool,
    /// Model-specific effort level (Qwen3.8: low | medium | high | xhigh). None = template default.
    #[serde(default)]
    pub effort: Option<String>,
    /// Most reasoning tokens before the model is made to close its reasoning and answer (the
    /// thinking budget). None = the model plugin's default for the effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// The user is waiting (foreground chat). Preempts everything else.
    Foreground,
    /// A working agent.
    #[default]
    Agent,
    /// Idle-time work (night shift, descriptions, prewarm).
    Background,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatParams {
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// OpenAI-style tool_choice (`"auto"`, `"none"`, `"required"`, or a named function).
    #[serde(default)]
    pub tool_choice: Option<Value>,
    /// GBNF grammar constraining the output.
    #[serde(default)]
    pub grammar: Option<String>,
    /// JSON Schema constraining the output (converted to a grammar by the engine).
    #[serde(default)]
    pub json_schema: Option<Value>,
    #[serde(default)]
    pub sampling: Sampling,
    #[serde(default)]
    pub thinking: Thinking,
    #[serde(default)]
    pub priority: Priority,
    /// Conversation identity for prefix-cache affinity (session or agent id).
    #[serde(default)]
    pub cache_key: Option<String>,
    #[serde(default)]
    pub stop: Vec<String>,
    /// None = no cap (the model stops by itself or at context end).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Work nobody waits for (learning after a turn): it starts only while the model serves no
    /// other request, only in a free slot, never drops another conversation's cache for room, and
    /// leaves nothing in the pool when it ends. Once running, any other request goes first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idle_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompleteParams {
    pub prompt: String,
    #[serde(default)]
    pub grammar: Option<String>,
    #[serde(default)]
    pub sampling: Sampling,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub cache_key: Option<String>,
    #[serde(default)]
    pub stop: Vec<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Return the top-n token probabilities per generated token.
    #[serde(default)]
    pub n_probs: Option<u32>,
}

/// One streamed piece of output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatDelta {
    /// content | reasoning | tool_call
    pub kind: String,
    pub text: String,
    /// For `tool_call`: index of the call being streamed.
    #[serde(default)]
    pub index: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    /// Prompt tokens served from cache (not re-processed).
    pub cached_tokens: u32,
    pub completion_tokens: u32,
    pub prefill_ms: f64,
    pub decode_ms: f64,
    /// Time the request waited for a slot.
    #[serde(default)]
    pub queue_ms: f64,
    #[serde(default)]
    pub draft_accepted: u32,
    #[serde(default)]
    pub draft_total: u32,
    #[serde(default)]
    pub slot: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatResult {
    pub content: String,
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// stop | length | tool_calls | cancelled | repeated | error
    pub finish_reason: String,
    pub usage: Usage,
    #[serde(default)]
    pub probs: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenizeParams {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenizeResult {
    pub tokens: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CountResult {
    pub tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlotState {
    pub slot: u32,
    #[serde(default)]
    pub cache_key: Option<String>,
    pub busy: bool,
    pub n_tokens: u32,
    #[serde(default)]
    pub priority: Option<Priority>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlotsResult {
    pub slots: Vec<SlotState>,
    pub queued: u32,
    #[serde(default)]
    pub hibernated: Vec<String>,
    /// Queued requests per priority class.
    #[serde(default)]
    pub queued_by_priority: QueueDepth,
    /// Slots running a request.
    #[serde(default)]
    pub busy: u32,
    /// Cells of the unified KV pool the running requests share.
    #[serde(default)]
    pub pool_tokens: u64,
    /// Cells reserved by running requests (prompt plus generation reserve); a request waits for room.
    #[serde(default)]
    pub reserved_tokens: u64,
    /// Recent throughput and the measured cost of hibernation (for the agents scheduler).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<LiveMetrics>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct QueueDepth {
    pub foreground: u32,
    pub agent: u32,
    pub background: u32,
}

/// Throughput over a recent window of completed requests, and hibernation costs measured so far.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LiveMetrics {
    /// Length of the window (seconds) the rates cover.
    pub window_s: f64,
    /// Requests completed in the window.
    pub requests: u32,
    /// Generated tokens per second across all slots in the window.
    pub decode_tps_aggregate: f64,
    /// Mean per-request decode rate (tokens / engine decode time).
    pub decode_tps_per_stream: f64,
    /// Prompt tokens processed (not served from cache) per second of engine prefill time.
    pub prefill_tps: f64,
    /// Share of prompt tokens served from cache in the window.
    pub cache_hit_ratio: f64,
    pub mean_queue_ms: f64,
    #[serde(default)]
    pub hibernation: HibernationCost,
}

/// Measured cost of `model.hibernate` and of the automatic restore (means over this model load).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HibernationCost {
    pub saves: u32,
    pub restores: u32,
    /// Bytes written per token of saved context (KV, recurrent state and checkpoints).
    pub bytes_per_token: f64,
    pub save_ms_mean: f64,
    pub restore_ms_mean: f64,
    /// Disk throughput of saves and restores, MB/s.
    pub save_mb_s: f64,
    pub restore_mb_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheKeyParams {
    pub cache_key: String,
}

/// `model.hold`: keep `cache_key` resident while its turn runs, or release it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HoldParams {
    pub cache_key: String,
    pub hold: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrewarmParams {
    pub cache_key: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub thinking: Thinking,
    /// Prefill only the leading system block, for conversations not created yet that will start
    /// with it: the model keeps it for any cache key and keeps no conversation under `cache_key`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared_only: bool,
    /// Prefill only while the model serves no other conversation: it waits while another
    /// conversation's request runs or waits, and such a request stops it at its next step (what it
    /// prefilled is kept). For a guess that the conversation continues (a turn just ended), which
    /// must never slow work under way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idle_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Variant {
    pub id: String,
    pub label: String,
    pub path: String,
    pub installed: bool,
    pub active: bool,
    #[serde(default)]
    pub notes: String,
    /// Hugging Face repository the file comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Pinned repository revision (commit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Expected SHA-256 of the file (Hugging Face `lfs.oid`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Whether the installed file's SHA-256 was computed and matched (None = not installed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Whether the file embeds an MTP (NextN) draft head (read from its header when installed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtp: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VariantsResult {
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SelectParams {
    pub variant: String,
}

/// Result of `model.select`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SelectResult {
    pub variant: String,
    pub previous: String,
    /// False when the requested variant was already active (nothing reloaded).
    pub changed: bool,
    /// Wall time of the switch: drain, unload, load.
    pub reload_ms: f64,
    pub unload_ms: f64,
    pub load_ms: f64,
    pub info: ModelInfo,
}
