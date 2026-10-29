//! Fit the engine to measured memory, once per load (the rule the model plugins share).
//!
//! The budget is the smaller of the Metal working set (`recommendedMaxWorkingSetSize`) and physical
//! RAM less a quarter (left for the application, the browser and the system). Metal memory is
//! wired: macOS cannot page or compress it and raises no memory pressure for it, so the budget is a
//! hard ceiling the model never passes (the uzu plugin's measurements: a tenth held back drove a
//! 48 GB Mac into memory pressure with a 27B model, a quarter did not).
//!
//! llama.cpp allocates everything at load and clears it: the weights, one unified KV pool shared by
//! every sequence (any one may grow to all of it), one recurrent state (Gated DeltaNet) per
//! sequence, and the compute buffers. Context checkpoints (copies of a recurrent state, the points
//! a conversation rolls back to) live in host memory, which on a Mac is the same memory, so they
//! count too. So the plan is sized to what the conversations use, never to the budget: a pool
//! planned up to the budget is wired in full at load whether used or not (uzu commits its pool as
//! conversations grow; llama.cpp cannot). On a 48 GB Mac the budget rule held 22 GB for a 7.2 GB
//! model. This engine is the light one: everything together stays under [`MAX_TOTAL_BYTES`] on
//! every Mac, and the KV pool takes what the rest leaves under it. Every term comes from the GGUF
//! header; what the load really took is reported beside the plan (`model.info.extra.fit.measured`).
//!
//! A file with an MTP draft layer adds three terms: one more copy of each sequence's recurrent
//! state (the snapshot a rejected draft rolls back to), the draft layer's own KV for every pool
//! token, and the draft context's compute buffers.

use crate::llama::{KvType, Model};
use serde::Serialize;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// A quarter of physical RAM stays outside the budget, for everything else on the machine.
pub const RAM_RESERVE_DIVISOR: u64 = 4;
/// Pool sizes are whole multiples of this many tokens.
pub const CONTEXT_GRANULARITY: u64 = 1024;
/// Everything the engine holds, at most: weights, compute buffers, recurrent states, checkpoints
/// and the KV pool (the owner's ceiling for the light engine).
pub const MAX_TOTAL_BYTES: u64 = 12_000_000_000;
/// Compute buffers of one forward pass at the plan's micro-batch (measured: 1,030 MiB on the GPU
/// and 168 MiB on the CPU at a 1024-token micro-batch; they scale with it, and the plan uses 256)
/// plus headroom.
pub const COMPUTE_RESERVE: u64 = GIB / 2;
/// Sequences kept for shared leading blocks (a sequence holds a recurrent state whether used or
/// not). One: the open project's chat block; others come back from disk in a fraction of a second.
pub const SEEDS: u64 = 1;
/// The most the shared pool takes: 131,072 tokens of 8-bit KV (4.25 GiB on the 27B). Under
/// [`MAX_TOTAL_BYTES`] it gets what the rest leaves (PQ2_0: about 80k tokens);
/// any resident conversation may use all of it.
pub const POOL_TOKENS: u64 = 131_072;
/// The pool the plan aims for before it gives up resident conversations.
pub const TARGET_POOL_TOKENS: u64 = 65_536;
/// The KV cache type: 8-bit (half of f16; the context, not the weights).
pub const KV_TYPE: KvType = KvType::Q8_0;
/// Tokens per decode call (a prefill step).
pub const BATCH: u32 = 1024;
/// Tokens per compute pass: 256 prefills about as fast as 512 (measured on the 27B: 358 against 364
/// tok/s; 342 at 1024) with a quarter of the 1024 compute buffers.
pub const MICRO_BATCH: u32 = 256;
/// Tokens the MTP head drafts per step. One: a GPU without the M5's tensor units checks two tokens
/// at 1.42x the cost of one and three at 1.91x, so only the first draft pays (measured on the 27B
/// with the tensor units off: code 24 -> 33 tok/s with one draft, 30 with two, 28 with three).
pub const DRAFT_TOKENS: u32 = 1;
/// Compute buffers of the MTP draft context (one layer at the plan's micro-batch, the head's
/// logits for one row) and the target's hidden-state rows for a full batch, with headroom.
pub const DRAFT_COMPUTE_RESERVE: u64 = GIB / 4;

/// Memory geometry of a loaded GGUF (`qwen35`: Gated DeltaNet layers with full attention every
/// `full_attention_interval` layers).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Geometry {
    pub weights_bytes: u64,
    pub attention_layers: u64,
    pub recurrent_layers: u64,
    pub kv_heads: u64,
    pub key_length: u64,
    pub value_length: u64,
    /// One sequence's recurrent state (conv and SSM states, f32).
    pub recurrent_bytes: u64,
    pub native_context: u64,
    /// MTP (NextN) draft layers in the file.
    pub mtp_layers: u64,
}

impl Geometry {
    /// Whether the file drafts with its own MTP layer.
    pub fn drafts(&self) -> bool {
        self.mtp_layers > 0
    }
}

impl Geometry {
    pub fn read(model: &Model) -> Result<Geometry, String> {
        let arch = model.meta("general.architecture").ok_or("the GGUF names no architecture")?;
        let key = |name: &str| format!("{arch}.{name}");
        let need = |name: &str| model.meta_u64(&key(name)).ok_or_else(|| format!("the GGUF has no `{}`", key(name)));
        // `block_count` includes the MTP layers, which sit after the trunk.
        let mtp_layers = model.meta_u64(&key("nextn_predict_layers")).unwrap_or(0);
        let blocks = need("block_count")?.saturating_sub(mtp_layers);
        // A plain transformer has no interval: every layer has attention.
        let interval = model.meta_u64(&key("full_attention_interval")).unwrap_or(1).max(1);
        let attention_layers = blocks / interval;
        let recurrent_layers = blocks - attention_layers;
        let recurrent_bytes = if recurrent_layers == 0 {
            0
        } else {
            let conv_kernel = need("ssm.conv_kernel")?;
            let inner = need("ssm.inner_size")?;
            let state = need("ssm.state_size")?;
            let heads = need("ssm.time_step_rank")?.max(1);
            let groups = need("ssm.group_count")?;
            // SSM state: heads × key dim × value dim; conv state: (kernel − 1) × channels (the value
            // projection plus the query and key projections of every group).
            let ssm = heads * state * (inner / heads) * 4;
            let conv = (conv_kernel.saturating_sub(1)) * (inner + 2 * groups * state) * 4;
            recurrent_layers * (ssm + conv)
        };
        Ok(Geometry {
            weights_bytes: model.size(),
            attention_layers,
            recurrent_layers,
            kv_heads: need("attention.head_count_kv")?,
            key_length: need("attention.key_length")?,
            value_length: need("attention.value_length")?,
            recurrent_bytes,
            native_context: u64::from(model.n_ctx_train()).max(need("context_length").unwrap_or(0)),
            mtp_layers,
        })
    }

    /// Attention KV bytes per token at `kv` (q8_0: 34 bytes per 32 values), the MTP layer's
    /// own cache included (an attention layer of the same shape).
    pub fn kv_bytes_per_token(&self, kv: KvType) -> u64 {
        let values = (self.attention_layers + self.mtp_layers) * self.kv_heads * (self.key_length + self.value_length);
        match kv {
            KvType::F16 => values * 2,
            KvType::Q8_0 => values * 34 / 32,
        }
    }
}

/// The memory the plan fits in.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Budget {
    pub working_set: u64,
    pub physical: u64,
    /// The operator's limit (`memory_limit_bytes`).
    pub limit: Option<u64>,
}

impl Budget {
    pub fn usable(&self) -> u64 {
        let ram = self.physical - self.physical / RAM_RESERVE_DIVISOR;
        let machine = if self.working_set > 0 { self.working_set.min(ram) } else { ram };
        self.limit.map_or(machine, |limit| limit.min(machine))
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Plan {
    pub budget: u64,
    pub kv_type: KvType,
    pub kv_bytes_per_token: u64,
    /// Tokens of the shared pool (`n_ctx`), at most the native context.
    pub ctx_total: u64,
    /// Conversations resident at once (their sequences and recurrent states).
    pub resident: u64,
    /// Context checkpoints kept per resident conversation.
    pub checkpoints: u64,
    /// Sequences of the context: the resident conversations and the shared leading blocks.
    pub sequences: u64,
    pub weights_bytes: u64,
    pub recurrent_bytes: u64,
    pub compute_reserve: u64,
    /// Tokens drafted per step by the file's MTP layer (0: no drafting).
    pub draft_tokens: u32,
    /// The rollback snapshots and the draft context's compute buffers.
    pub draft_bytes: u64,
    /// Bytes the plan reaches in all.
    pub total_bytes: u64,
}

/// Resident conversations and checkpoints each, most preferred first: the chat, an agent and a
/// spare. A call makes three reuse points (the end of the leading system block, the history before
/// the last message, the end of the prompt); with two kept, the leading block's was dropped within
/// a turn and the next turn read the whole prompt again. Four keep it and a spare; fewer
/// conversations come before fewer checkpoints, and two only where nothing else fits.
const SHAPES: &[(u64, u64)] = &[(3, 4), (2, 4), (1, 4), (1, 3), (1, 2)];

fn plan_for(geometry: &Geometry, budget: u64, kv: KvType, resident: u64, checkpoints: u64) -> Option<Plan> {
    let sequences = resident + SEEDS;
    let draft_tokens = if geometry.drafts() { DRAFT_TOKENS } else { 0 };
    let draft_bytes = if geometry.drafts() { geometry.recurrent_bytes * sequences * u64::from(DRAFT_TOKENS) + DRAFT_COMPUTE_RESERVE } else { 0 };
    let fixed = geometry.weights_bytes + COMPUTE_RESERVE + geometry.recurrent_bytes * (sequences + resident * checkpoints) + draft_bytes;
    let room = budget.checked_sub(fixed)?;
    let per_token = geometry.kv_bytes_per_token(kv).max(1);
    let tokens = (room / per_token).min(POOL_TOKENS).min(geometry.native_context) / CONTEXT_GRANULARITY * CONTEXT_GRANULARITY;
    (tokens > 0).then(|| Plan {
        budget,
        kv_type: kv,
        kv_bytes_per_token: per_token,
        ctx_total: tokens,
        resident,
        checkpoints,
        sequences,
        weights_bytes: geometry.weights_bytes,
        recurrent_bytes: geometry.recurrent_bytes,
        compute_reserve: COMPUTE_RESERVE,
        draft_tokens,
        draft_bytes,
        total_bytes: fixed + tokens * per_token,
    })
}

/// The plan, within the machine's budget and [`MAX_TOTAL_BYTES`]: the first shape whose pool
/// reaches [`TARGET_POOL_TOKENS`] (or the native context, when smaller); on a machine without room
/// for that, of the shapes that give one conversation `min_context` tokens the one with the most
/// checkpoints and then the largest pool (a 16 GB Mac: one conversation with a large pool, not two
/// in a small one). None: this machine cannot hold the model with `min_context`.
pub fn fit(geometry: &Geometry, budget: &Budget, min_context: u64) -> Option<Plan> {
    let usable = budget.usable().min(MAX_TOTAL_BYTES);
    let target = TARGET_POOL_TOKENS.min(geometry.native_context);
    let plans = || SHAPES.iter().filter_map(|&(r, c)| plan_for(geometry, usable, KV_TYPE, r, c));
    if let Some(plan) = plans().find(|plan| plan.ctx_total >= target) {
        return Some(plan);
    }
    let floor = min_context.min(geometry.native_context);
    plans().filter(|plan| plan.ctx_total >= floor).max_by_key(|plan| (plan.checkpoints, plan.ctx_total))
}
