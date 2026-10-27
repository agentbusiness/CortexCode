//! Fit the engine to measured memory, once per load (the rule the model plugins share).
//!
//! Every term is read from the model directory (weights, the attention KV and recurrent-state
//! geometry of `config.json` and `speculator/config.json`) or measured on this machine. The budget
//! is the smallest of the Metal working set (`recommendedMaxWorkingSetSize`), physical RAM less a
//! quarter (left for the application, the browser and the system), and the kernel's wired-memory
//! limit less an eighth of RAM. Metal memory is wired: macOS cannot page or compress it and raises
//! no memory pressure for it, so the budget is a hard ceiling the model never passes. Without the
//! RAM reserve the plan reached the whole working set (40.2 GB of a 48 GB Mac), the model held
//! 41.6 GB and the GPU driver panicked the machine; we measured that a tenth held back drove a
//! 48 GB Mac into memory pressure with a 27B model, a quarter did not.
//!
//! The wired limit (`vm.global_user_wire_limit`, 40.7 GB on a 48 GB Mac) bounds all wired memory
//! together: the kernel's own, the GPU memory of the window server, the app and the browsers, and
//! the model's. A wiring past it fails (`kIOReturnNoMemory`) and the GPU driver panics on that
//! failure: seen at a 193k-token conversation, the model near 37 GB within its 38.7 GB plan, memory
//! otherwise fine. An eighth of RAM stays below the limit for everyone else's wired memory (4.3 GB
//! was wired with no model loaded).
//!
//! uzu keeps every conversation's attention KV in sparse Metal buffers whose pages are committed as
//! the conversation grows, so the KV pool is one budget shared by all conversations: any one of them
//! may grow to the whole pool, and the scheduler admits work only while the resident conversations
//! fit. Each resident conversation also holds a fixed recurrent state (Gated DeltaNet) and context
//! checkpoints of it (reuse points the scheduler rolls back to, see `engine.rs`).

use serde::Serialize;
use serde_json::Value;
use std::path::Path;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// A quarter of physical RAM stays outside the budget, for everything else on the machine.
pub const RAM_RESERVE_DIVISOR: u64 = 4;
/// An eighth of physical RAM stays below the kernel's wired limit, for the wired memory of the
/// kernel and every other GPU user.
pub const WIRE_RESERVE_DIVISOR: u64 = 8;
/// Pool sizes are whole multiples of this many tokens.
pub const CONTEXT_GRANULARITY: u64 = 1024;
/// Rows every attention cache keeps beyond its prefix for the suffix being processed (uzu's
/// `ATTENTION_SUFFIX_CAPACITY`).
pub const SUFFIX_ROWS: u64 = 1024;
/// Scratch for one forward pass: activations of a 1024-token prefill batch, the speculator's
/// draft tree and the sampling buffers. Bounded with headroom over the device's peak allocation
/// measured after a load (`model.info.extra.fit.measured`).
pub const COMPUTE_RESERVE: u64 = 3 * GIB;
/// Context checkpoints a resident conversation keeps. A chat call checkpoints the end of the
/// history it shares with the next call and the end of its own prompt (and the leading block), so
/// one checkpoint per conversation loses the first whenever the second is taken (a
/// measured bound).
pub const MAX_CHECKPOINTS: u64 = 8;
pub const MIN_CHECKPOINTS: u64 = 2;

/// Memory geometry of a uzu model directory.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Geometry {
    pub weights_bytes: u64,
    /// Attention KV bytes per token across the target model's attention layers.
    pub kv_bytes_per_token: u64,
    /// Attention KV bytes per token of the speculator's context.
    pub draft_kv_bytes_per_token: u64,
    /// Recurrent state of one conversation (Gated DeltaNet conv and SSM states, f32).
    pub recurrent_bytes: u64,
    pub attention_layers: u64,
    pub recurrent_layers: u64,
    pub native_context: u64,
    pub has_speculator: bool,
}

fn layer_configs(value: &Value) -> Vec<&Value> {
    value.as_array().map(|layers| layers.iter().collect()).unwrap_or_default()
}

/// K + V bytes per token of the owned attention layers (bf16), and the count of such layers.
fn attention_kv(layers: &[&Value]) -> (u64, u64) {
    let mut bytes = 0;
    let mut count = 0;
    for layer in layers {
        let mixer = &layer["mixer_config"];
        let shared = !layer["kv_source_layer_index"].is_null();
        if mixer["type"] == "AttentionConfig" && !shared {
            let groups = mixer["num_groups"].as_u64().unwrap_or(0);
            let head_dim = mixer["head_dim"].as_u64().unwrap_or(0);
            bytes += 2 * groups * head_dim * 2;
            count += 1;
        }
    }
    (bytes, count)
}

impl Geometry {
    pub fn read(dir: &Path) -> Result<Self, String> {
        let read = |name: &str| -> Result<Value, String> {
            let path = dir.join(name);
            let text = std::fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            serde_json::from_slice(&text).map_err(|error| format!("cannot parse {}: {error}", path.display()))
        };
        let config = read("config.json")?;
        let layers = layer_configs(&config["decoder_config"]["transformer_config"]["layer_configs"]);
        if layers.is_empty() {
            return Err("config.json has no transformer layers".into());
        }
        let (kv_bytes_per_token, attention_layers) = attention_kv(&layers);
        let mut recurrent_bytes = 0;
        let mut recurrent_layers = 0;
        let mut native_context = 0;
        for layer in &layers {
            let mixer = &layer["mixer_config"];
            if mixer["type"] == "DeltaNetConfig" {
                let heads = mixer["num_heads"].as_u64().unwrap_or(0);
                let groups = mixer["num_groups"].as_u64().unwrap_or(0);
                let head_dim = mixer["head_dim"].as_u64().unwrap_or(0);
                let value_dim = mixer["value_head_dim"].as_u64().unwrap_or(0);
                let kernel = mixer["kernel_size"].as_u64().unwrap_or(1);
                let conv_dim = 2 * groups * head_dim + heads * value_dim;
                recurrent_bytes += 4 * (heads * value_dim * head_dim + conv_dim * kernel.saturating_sub(1));
                recurrent_layers += 1;
            }
            if let Some(length) = layer["rope_config"]["max_sequence_length"].as_u64() {
                native_context = native_context.max(length);
            }
        }
        let speculator_dir = dir.join("speculator");
        let has_speculator = speculator_dir.join("config.json").exists();
        let draft_kv_bytes_per_token = if has_speculator {
            let speculator = read("speculator/config.json")?;
            let layers = layer_configs(&speculator["speculator_config"]["draft_config"]["layer_configs"]);
            attention_kv(&layers).0
        } else {
            0
        };
        let file_bytes = |path: &Path| std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        let weights_bytes = file_bytes(&dir.join("model.safetensors"))
            + if has_speculator { file_bytes(&speculator_dir.join("model.safetensors")) } else { 0 };
        if native_context == 0 {
            return Err("config.json declares no context length".into());
        }
        Ok(Self {
            weights_bytes,
            kv_bytes_per_token,
            draft_kv_bytes_per_token,
            recurrent_bytes,
            attention_layers,
            recurrent_layers,
            native_context,
            has_speculator,
        })
    }

    pub fn bytes_per_token(&self) -> u64 {
        self.kv_bytes_per_token + self.draft_kv_bytes_per_token
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Budget {
    /// Metal `recommendedMaxWorkingSetSize`; zero when unknown (RAM alone bounds).
    pub device_bytes: u64,
    /// Physical RAM less [`RAM_RESERVE_DIVISOR`]'s share (or the operator limit, when lower). Read
    /// from the machine, never from what happens to be free at load.
    pub ram_bytes: u64,
    /// The kernel's wired-memory limit less [`WIRE_RESERVE_DIVISOR`]'s share of RAM; zero when the
    /// limit is unknown.
    #[serde(default)]
    pub wire_bytes: u64,
    pub physical_bytes: u64,
}

impl Budget {
    pub fn new(device_bytes: u64, physical_bytes: u64, wire_limit: u64, limit: Option<u64>) -> Self {
        let ram = physical_bytes - physical_bytes / RAM_RESERVE_DIVISOR;
        let wire_bytes = if wire_limit == 0 { 0 } else { wire_limit.saturating_sub(physical_bytes / WIRE_RESERVE_DIVISOR) };
        Self { device_bytes, ram_bytes: limit.map_or(ram, |limit| ram.min(limit)), wire_bytes, physical_bytes }
    }

    pub fn usable(&self) -> u64 {
        [self.device_bytes, self.wire_bytes].into_iter().filter(|bytes| *bytes > 0).fold(self.ram_bytes, u64::min)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Plan {
    /// Tokens in the shared pool; any one conversation may use all of it (up to native). The
    /// resident conversations' fixed states are taken from it too ([`Plan::conversation_tokens`]).
    pub ctx_total: u64,
    pub ctx_native: u64,
    /// Context checkpoints per resident conversation.
    pub checkpoints: u64,
    /// Pool tokens one resident conversation's fixed state stands for: its recurrent state, its
    /// checkpoints and its suffix rows. Charged when it is admitted, released when it is spilled.
    pub conversation_tokens: u64,
    pub weights_bytes: u64,
    pub kv_bytes: u64,
    pub bytes_per_token: u64,
    pub recurrent_bytes: u64,
    pub checkpoint_bytes: u64,
    pub compute_bytes: u64,
    /// Bytes the plan may reach with the pool full.
    pub total_bytes: u64,
    pub budget: Budget,
    pub within_budget: bool,
}

impl Plan {
    /// Conversations the pool holds resident at once when each holds no context yet: an upper
    /// bound, not a limit (conversations that hold context leave room for fewer).
    pub fn resident_capacity(&self) -> u64 {
        (self.ctx_total / self.conversation_tokens.max(1)).max(1)
    }
}

/// The largest shared pool within the budget. How many conversations are resident at once is not
/// planned: each one's fixed state ([`Plan::conversation_tokens`]) is taken from the pool when it
/// is admitted, so they share one memory with their contexts. One conversation's checkpoints take
/// at most a sixteenth of the room the weights and scratch leave, within
/// [`MIN_CHECKPOINTS`]..=[`MAX_CHECKPOINTS`].
pub fn fit(geometry: &Geometry, budget: &Budget) -> Plan {
    let per_token = geometry.bytes_per_token().max(1);
    let fixed = geometry.weights_bytes + COMPUTE_RESERVE;
    let room = budget.usable().saturating_sub(fixed);
    let checkpoints = if geometry.recurrent_bytes == 0 {
        MAX_CHECKPOINTS
    } else {
        (room / 16 / geometry.recurrent_bytes).clamp(MIN_CHECKPOINTS, MAX_CHECKPOINTS)
    };
    let ctx_total = (room / per_token) / CONTEXT_GRANULARITY * CONTEXT_GRANULARITY;
    let kv_bytes = ctx_total * per_token;
    let total = fixed + kv_bytes;
    Plan {
        ctx_total,
        ctx_native: geometry.native_context,
        checkpoints,
        conversation_tokens: conversation_tokens(geometry, checkpoints),
        weights_bytes: geometry.weights_bytes,
        kv_bytes,
        bytes_per_token: per_token,
        recurrent_bytes: geometry.recurrent_bytes,
        checkpoint_bytes: geometry.recurrent_bytes,
        compute_bytes: COMPUTE_RESERVE,
        total_bytes: total,
        budget: *budget,
        within_budget: total <= budget.usable(),
    }
}

/// Pool tokens of one resident conversation's fixed state: its recurrent state and `checkpoints`
/// copies of it, plus the suffix rows its attention cache keeps mapped beside its prefix.
pub fn conversation_tokens(geometry: &Geometry, checkpoints: u64) -> u64 {
    let per_token = geometry.bytes_per_token().max(1);
    ((1 + checkpoints) * geometry.recurrent_bytes).div_ceil(per_token) + SUFFIX_ROWS
}
