//! The vendored PrismML llama.cpp through the shim's flat C ABI (`native/shim.cpp`): a loaded
//! model and its context. Every llama.cpp struct stays in C++; Rust holds opaque pointers.
//!
//! A [`Context`] is used by one thread (the engine thread); it keeps its [`Model`] alive.

use std::ffi::{CString, c_char, c_void};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;

unsafe extern "C" {
    fn bn_init(log_level: i32);
    fn bn_model_load(path: *const c_char, n_gpu_layers: i32, load_mtp: i32, error: *mut c_char, error_len: usize) -> *mut c_void;
    fn bn_model_free(model: *mut c_void);
    fn bn_model_meta(model: *const c_void, key: *const c_char, buf: *mut c_char, len: usize) -> i32;
    fn bn_model_size(model: *const c_void) -> u64;
    fn bn_model_n_params(model: *const c_void) -> u64;
    fn bn_model_n_ctx_train(model: *const c_void) -> i32;
    fn bn_model_n_layer_nextn(model: *const c_void) -> i32;
    fn bn_model_n_embd_out(model: *const c_void) -> i32;
    fn bn_vocab_n_tokens(model: *const c_void) -> i32;
    fn bn_vocab_is_eog(model: *const c_void, token: i32) -> i32;
    fn bn_tokenize(model: *const c_void, text: *const c_char, len: i32, out: *mut i32, cap: i32) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn bn_context_new(
        model: *mut c_void,
        n_ctx: u32,
        n_batch: u32,
        n_ubatch: u32,
        n_seq_max: u32,
        kv_type: i32,
        flash: i32,
        threads: i32,
        n_rs_seq: u32,
        mtp: i32,
        error: *mut c_char,
        error_len: usize,
    ) -> *mut c_void;
    fn bn_context_free(ctx: *mut c_void);
    fn bn_n_ctx(ctx: *const c_void) -> u32;
    fn bn_decode(ctx: *mut c_void, tokens: *const i32, n: i32, pos0: i32, seq: i32, logits: i32) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn bn_decode_embd(ctx: *mut c_void, tokens: *const i32, embd: *const f32, n: i32, n_embd: i32, pos0: i32, seq: i32, last_logits: i32) -> i32;
    fn bn_logits_last(ctx: *mut c_void) -> *const f32;
    fn bn_logits_ith(ctx: *mut c_void, i: i32) -> *const f32;
    fn bn_set_embeddings_nextn(ctx: *mut c_void, value: i32, masked: i32);
    fn bn_embeddings_nextn_ith(ctx: *mut c_void, i: i32) -> *const f32;
    fn bn_synchronize(ctx: *mut c_void);
    fn bn_seq_rm(ctx: *mut c_void, seq: i32, p0: i32, p1: i32) -> i32;
    fn bn_seq_cp(ctx: *mut c_void, from: i32, to: i32);
    fn bn_seq_pos_max(ctx: *mut c_void, seq: i32) -> i32;
    fn bn_state_size(ctx: *mut c_void, seq: i32, partial: i32) -> u64;
    fn bn_state_get(ctx: *mut c_void, dst: *mut u8, size: u64, seq: i32, partial: i32) -> u64;
    fn bn_state_set(ctx: *mut c_void, src: *const u8, size: u64, seq: i32, partial: i32) -> u64;
    fn bn_gpu_memory(free: *mut u64, total: *mut u64, name: *mut c_char, len: usize) -> i32;
}

/// The engine's revision (the vendored fork, see `vendor/llama.cpp/REVISION`).
pub const REVISION: &str = "PrismML llama.cpp prism 0082ac24a89ae62132041b40e2c0170e278a98f4";

fn error_text(buffer: &[u8]) -> String {
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// Load the backends (once per process). `log_level`: 0 debug, 1 info, 2 warn, 3 error.
pub fn init(log_level: i32) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the shim's init registers a log callback and initializes the backends; run once.
    ONCE.call_once(|| unsafe { bn_init(log_level) });
}

/// The GPU as Metal reports it.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Gpu {
    pub name: String,
    /// `recommendedMaxWorkingSetSize`.
    pub working_set_bytes: u64,
    /// `currentAllocatedSize`: what this process holds on the device.
    pub allocated_bytes: u64,
}

pub fn gpu() -> Option<Gpu> {
    let (mut free, mut total) = (0u64, 0u64);
    let mut name = vec![0u8; 256];
    // SAFETY: the out pointers are valid for writes; `name` is a buffer of the given length.
    let found = unsafe { bn_gpu_memory(&mut free, &mut total, name.as_mut_ptr().cast(), name.len()) };
    (found != 0).then(|| Gpu { name: error_text(&name), working_set_bytes: total, allocated_bytes: total.saturating_sub(free) })
}

pub struct Model {
    ptr: NonNull<c_void>,
}

// SAFETY: a llama_model is immutable after load and may be read from several threads.
unsafe impl Send for Model {}
unsafe impl Sync for Model {}

impl Model {
    /// Load a GGUF with its MTP draft layers, when it has them.
    pub fn load(path: &Path) -> Result<Arc<Model>, String> {
        let text = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| "model path contains a NUL byte".to_owned())?;
        let mut error = vec![0u8; 512];
        // SAFETY: the path is NUL-terminated; the error buffer has the given length.
        let ptr = unsafe { bn_model_load(text.as_ptr(), 999, 1, error.as_mut_ptr().cast(), error.len()) };
        NonNull::new(ptr).map(|ptr| Arc::new(Model { ptr })).ok_or_else(|| error_text(&error))
    }

    /// A GGUF metadata value as text.
    pub fn meta(&self, key: &str) -> Option<String> {
        let key = CString::new(key).ok()?;
        let mut buffer = vec![0u8; 4096];
        // SAFETY: the key is NUL-terminated; the buffer has the given length.
        let n = unsafe { bn_model_meta(self.ptr.as_ptr(), key.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
        (n >= 0).then(|| error_text(&buffer))
    }

    pub fn meta_u64(&self, key: &str) -> Option<u64> {
        self.meta(key)?.trim().parse().ok()
    }

    /// Bytes of the weights.
    pub fn size(&self) -> u64 {
        // SAFETY: a live model.
        unsafe { bn_model_size(self.ptr.as_ptr()) }
    }

    pub fn n_params(&self) -> u64 {
        // SAFETY: a live model.
        unsafe { bn_model_n_params(self.ptr.as_ptr()) }
    }

    pub fn n_ctx_train(&self) -> u32 {
        // SAFETY: a live model.
        u32::try_from(unsafe { bn_model_n_ctx_train(self.ptr.as_ptr()) }).unwrap_or(0)
    }

    /// MTP (NextN) draft layers loaded with the model.
    pub fn n_layer_nextn(&self) -> u32 {
        // SAFETY: a live model.
        u32::try_from(unsafe { bn_model_n_layer_nextn(self.ptr.as_ptr()) }).unwrap_or(0)
    }

    /// Floats in one hidden-state row (what the MTP head reads from the target).
    pub fn n_embd_out(&self) -> usize {
        // SAFETY: a live model.
        usize::try_from(unsafe { bn_model_n_embd_out(self.ptr.as_ptr()) }).unwrap_or(0)
    }

    pub fn n_vocab(&self) -> usize {
        // SAFETY: a live model.
        usize::try_from(unsafe { bn_vocab_n_tokens(self.ptr.as_ptr()) }).unwrap_or(0)
    }

    /// End of generation (`<|im_end|>`, `<|endoftext|>`).
    pub fn is_eog(&self, token: u32) -> bool {
        // SAFETY: a live model.
        unsafe { bn_vocab_is_eog(self.ptr.as_ptr(), token as i32) != 0 }
    }

    /// The GGUF vocab's own tokenization (special tokens parsed).
    pub fn tokenize(&self, text: &str) -> Vec<u32> {
        let Ok(len) = i32::try_from(text.len()) else { return Vec::new() };
        let mut out = vec![0i32; text.len() + 8];
        // SAFETY: `text` is valid for `len` bytes; `out` holds `out.len()` tokens.
        let mut n = unsafe { bn_tokenize(self.ptr.as_ptr(), text.as_ptr().cast(), len, out.as_mut_ptr(), out.len() as i32) };
        if n < 0 {
            out = vec![0i32; (-n) as usize];
            // SAFETY: as above, with the size the first call asked for.
            n = unsafe { bn_tokenize(self.ptr.as_ptr(), text.as_ptr().cast(), len, out.as_mut_ptr(), out.len() as i32) };
        }
        out.truncate(usize::try_from(n).unwrap_or(0));
        out.into_iter().map(|token| token as u32).collect()
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        // SAFETY: the model is freed once; every Context holding it has dropped its Arc first.
        unsafe { bn_model_free(self.ptr.as_ptr()) }
    }
}

/// KV cache element type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KvType {
    F16,
    Q8_0,
}

impl KvType {
    pub fn name(self) -> &'static str {
        match self {
            KvType::F16 => "f16",
            KvType::Q8_0 => "q8_0",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ContextParams {
    /// Cells of the unified KV pool (shared by every sequence; any one may use all of it).
    pub n_ctx: u32,
    /// Most tokens per decode call.
    pub n_batch: u32,
    /// Tokens per compute pass inside a decode.
    pub n_ubatch: u32,
    /// Sequences (each holds its own recurrent state, allocated up front).
    pub n_seq_max: u32,
    pub kv_type: KvType,
    /// Recurrent-state snapshots per sequence: the last this many tokens can be removed again (a
    /// rejected draft) without a checkpoint.
    pub n_rs_seq: u32,
    /// The MTP draft context of the model (its NextN layer only).
    pub mtp: bool,
}

/// Which rows of a decode keep their logits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Logits {
    None,
    Last,
    All,
}

pub struct Context {
    ptr: NonNull<c_void>,
    model: Arc<Model>,
    n_batch: usize,
    n_vocab: usize,
}

// SAFETY: a context is moved to the engine thread and used only there.
unsafe impl Send for Context {}

impl Context {
    pub fn new(model: Arc<Model>, params: ContextParams) -> Result<Context, String> {
        let mut error = vec![0u8; 512];
        let kv = match params.kv_type {
            KvType::F16 => 0,
            KvType::Q8_0 => 1,
        };
        // SAFETY: a live model; the error buffer has the given length.
        let ptr = unsafe {
            bn_context_new(
                model.ptr.as_ptr(),
                params.n_ctx,
                params.n_batch,
                params.n_ubatch,
                params.n_seq_max,
                kv,
                1,
                crate::machine::engine_threads(),
                params.n_rs_seq,
                i32::from(params.mtp),
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        let ptr = NonNull::new(ptr).ok_or_else(|| error_text(&error))?;
        let n_vocab = model.n_vocab();
        Ok(Context { ptr, model, n_batch: params.n_batch.max(1) as usize, n_vocab })
    }

    pub fn model(&self) -> &Arc<Model> {
        &self.model
    }

    pub fn n_ctx(&self) -> u32 {
        // SAFETY: a live context.
        unsafe { bn_n_ctx(self.ptr.as_ptr()) }
    }

    /// Feed `tokens` to sequence `seq` at positions `pos0..`, in calls of at most `n_batch`; the
    /// logits of the last token are kept when `last_logits`.
    pub fn decode(&mut self, tokens: &[u32], pos0: u32, seq: u32, last_logits: bool) -> Result<(), String> {
        let mut pos = pos0 as usize;
        let chunks = tokens.chunks(self.n_batch).count();
        for (index, chunk) in tokens.chunks(self.n_batch).enumerate() {
            let logits = if last_logits && index + 1 == chunks { Logits::Last } else { Logits::None };
            self.decode_batch(chunk, pos as u32, seq, logits)?;
            pos += chunk.len();
        }
        Ok(())
    }

    /// One decode call of at most `n_batch` tokens, with the rows `logits` asks for.
    pub fn decode_batch(&mut self, tokens: &[u32], pos0: u32, seq: u32, logits: Logits) -> Result<(), String> {
        let ids: Vec<i32> = tokens.iter().map(|token| *token as i32).collect();
        if ids.len() > self.n_batch {
            return Err(format!("a batch of {} tokens is over the context's {}", ids.len(), self.n_batch));
        }
        let mode = match logits {
            Logits::None => 0,
            Logits::Last => 1,
            Logits::All => 2,
        };
        // SAFETY: `ids` is valid for its length, which is at most n_batch.
        let status = unsafe { bn_decode(self.ptr.as_ptr(), ids.as_ptr(), ids.len() as i32, pos0 as i32, seq as i32, mode) };
        decode_status(status)
    }

    /// An MTP batch: row `i` is `tokens[i]` with the target's hidden state `rows[i]` (`n_embd`
    /// floats each) at the position before it.
    pub fn decode_rows(&mut self, tokens: &[u32], rows: &[f32], n_embd: usize, pos0: u32, seq: u32, last_logits: bool) -> Result<(), String> {
        let ids: Vec<i32> = tokens.iter().map(|token| *token as i32).collect();
        if ids.len() > self.n_batch || rows.len() != ids.len() * n_embd {
            return Err(format!("an MTP batch of {} rows does not fit (n_batch {}, {} floats)", ids.len(), self.n_batch, rows.len()));
        }
        // SAFETY: `ids` holds n tokens and `rows` n × n_embd floats; n is at most n_batch.
        let status = unsafe {
            bn_decode_embd(self.ptr.as_ptr(), ids.as_ptr(), rows.as_ptr(), ids.len() as i32, n_embd as i32, pos0 as i32, seq as i32, i32::from(last_logits))
        };
        decode_status(status)
    }

    /// The logits of the last decoded token that kept them.
    pub fn logits(&mut self) -> &[f32] {
        // SAFETY: after a decode with logits, the shim returns n_vocab floats owned by the context,
        // valid until the next decode (the borrow of `self` enforces it).
        unsafe {
            let ptr = bn_logits_last(self.ptr.as_ptr());
            if ptr.is_null() { &[] } else { std::slice::from_raw_parts(ptr, self.n_vocab) }
        }
    }

    /// The logits of row `i` of the last batch (it asked for them).
    pub fn logits_ith(&mut self, i: usize) -> &[f32] {
        // SAFETY: as `logits`, for a row of the last batch.
        unsafe {
            let ptr = bn_logits_ith(self.ptr.as_ptr(), i as i32);
            if ptr.is_null() { &[] } else { std::slice::from_raw_parts(ptr, self.n_vocab) }
        }
    }

    /// Keep the hidden state the MTP head reads: for every row of each batch (`masked` false) or
    /// only for rows with logits.
    pub fn set_embeddings_nextn(&mut self, value: bool, masked: bool) {
        // SAFETY: a live context.
        unsafe { bn_set_embeddings_nextn(self.ptr.as_ptr(), i32::from(value), i32::from(masked)) }
    }

    /// The hidden state of row `i` of the last batch (`n_embd` floats), empty when there is none.
    pub fn hidden_ith(&mut self, i: usize, n_embd: usize) -> &[f32] {
        // SAFETY: the context owns n_embd floats per row of the last batch, valid until the next
        // decode (the borrow of `self` enforces it).
        unsafe {
            let ptr = bn_embeddings_nextn_ith(self.ptr.as_ptr(), i as i32);
            if ptr.is_null() { &[] } else { std::slice::from_raw_parts(ptr, n_embd) }
        }
    }

    pub fn synchronize(&mut self) {
        // SAFETY: a live context.
        unsafe { bn_synchronize(self.ptr.as_ptr()) }
    }

    /// Remove positions `[p0, p1)` of `seq` (`-1`: open end). False when the memory cannot (a
    /// recurrent state cannot be cut in the middle: restore a checkpoint first).
    pub fn seq_rm(&mut self, seq: u32, p0: i32, p1: i32) -> bool {
        // SAFETY: a live context.
        unsafe { bn_seq_rm(self.ptr.as_ptr(), seq as i32, p0, p1) != 0 }
    }

    /// Make `to` a copy of `from` (attention cells are shared, the recurrent state copied).
    pub fn seq_cp(&mut self, from: u32, to: u32) {
        // SAFETY: a live context.
        unsafe { bn_seq_cp(self.ptr.as_ptr(), from as i32, to as i32) }
    }

    /// The last position `seq` holds, -1 when empty.
    pub fn seq_pos_max(&mut self, seq: u32) -> i32 {
        // SAFETY: a live context.
        unsafe { bn_seq_pos_max(self.ptr.as_ptr(), seq as i32) }
    }

    /// The state of `seq`: the recurrent part only (`partial`, a context checkpoint) or all of it.
    pub fn state(&mut self, seq: u32, partial: bool) -> Vec<u8> {
        // SAFETY: a live context; the buffer has the size the context reported.
        unsafe {
            let size = bn_state_size(self.ptr.as_ptr(), seq as i32, i32::from(partial));
            let mut buffer = vec![0u8; size as usize];
            let got = bn_state_get(self.ptr.as_ptr(), buffer.as_mut_ptr(), size, seq as i32, i32::from(partial));
            buffer.truncate(got as usize);
            buffer
        }
    }

    /// Restore a state taken by [`Context::state`] into `seq`. False when it did not apply.
    pub fn set_state(&mut self, seq: u32, partial: bool, state: &[u8]) -> bool {
        // SAFETY: a live context; `state` is valid for its length.
        unsafe { bn_state_set(self.ptr.as_ptr(), state.as_ptr(), state.len() as u64, seq as i32, i32::from(partial)) != 0 }
    }
}

fn decode_status(status: i32) -> Result<(), String> {
    match status {
        0 => Ok(()),
        1 => Err("the KV pool has no room for this batch".to_owned()),
        2 => Err("decoding was aborted".to_owned()),
        other => Err(format!("llama_decode failed ({other})")),
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: the context is freed once, before its model (it holds the model's Arc).
        unsafe { bn_context_free(self.ptr.as_ptr()) }
    }
}
