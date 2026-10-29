// The flat C ABI the Rust engine calls (src/llama.rs). Every llama.cpp struct is built and read
// here, so Rust passes and receives only pointers, integers and floats. One context is used by one
// thread at a time (the engine thread); the shim adds no locking.
#include "ggml-backend.h"
#include "llama.h"
#include "llama-ext.h"  // staging API: the MTP head's hidden-state rows (llama_set_embeddings_nextn)
#include "mtmd.h"

#include <cstdarg>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

namespace {

int g_log_level = GGML_LOG_LEVEL_WARN;

void log_callback(ggml_log_level level, const char * text, void *) {
    // CONT lines continue the previous message at its level.
    static ggml_log_level last = GGML_LOG_LEVEL_INFO;
    if (level != GGML_LOG_LEVEL_CONT) {
        last = level;
    }
    if (last >= g_log_level) {
        fputs(text, stderr);
    }
}

void set_error(char * error, size_t len, const std::string & message) {
    if (error && len > 0) {
        snprintf(error, len, "%s", message.c_str());
    }
}

}  // namespace

extern "C" {

// Load the backends once; `log_level`: 0 debug, 1 info, 2 warn, 3 error (llama.cpp's own log goes
// to stderr, which the frame writes to the plugin's log).
void bn_init(int32_t log_level) {
    g_log_level = GGML_LOG_LEVEL_DEBUG + log_level;
    llama_log_set(log_callback, nullptr);
    llama_backend_init();
}

// `load_mtp`: also load the file's MTP (NextN) draft layers, when it has them.
void * bn_model_load(const char * path, int32_t n_gpu_layers, int32_t load_mtp, char * error, size_t error_len) {
    auto params = llama_model_default_params();
    params.n_gpu_layers = n_gpu_layers;
    params.load_mtp = load_mtp != 0;
    llama_model * model = llama_model_load_from_file(path, params);
    if (!model) {
        set_error(error, error_len, std::string("llama.cpp could not load ") + path + " (see the plugin log)");
    }
    return model;
}

void bn_model_free(void * model) {
    llama_model_free(static_cast<llama_model *>(model));
}

// A metadata value as text; returns its length, or -1 when the key is absent.
int32_t bn_model_meta(const void * model, const char * key, char * buf, size_t len) {
    return llama_model_meta_val_str(static_cast<const llama_model *>(model), key, buf, len);
}

uint64_t bn_model_size(const void * model) {
    return llama_model_size(static_cast<const llama_model *>(model));
}

uint64_t bn_model_n_params(const void * model) {
    return llama_model_n_params(static_cast<const llama_model *>(model));
}

int32_t bn_model_n_ctx_train(const void * model) {
    return llama_model_n_ctx_train(static_cast<const llama_model *>(model));
}

// MTP (NextN) draft layers loaded with the model.
int32_t bn_model_n_layer_nextn(const void * model) {
    return llama_model_n_layer_nextn(static_cast<const llama_model *>(model));
}

// Width of one hidden-state row (what the MTP head reads from the target).
int32_t bn_model_n_embd_out(const void * model) {
    return llama_model_n_embd_out(static_cast<const llama_model *>(model));
}

int32_t bn_vocab_n_tokens(const void * model) {
    return llama_vocab_n_tokens(llama_model_get_vocab(static_cast<const llama_model *>(model)));
}

int32_t bn_vocab_is_eog(const void * model, int32_t token) {
    return llama_vocab_is_eog(llama_model_get_vocab(static_cast<const llama_model *>(model)), token) ? 1 : 0;
}

// The model's own tokenizer (special tokens parsed); returns the count, negative when `cap` is too
// small (the count needed, negated).
int32_t bn_tokenize(const void * model, const char * text, int32_t len, int32_t * out, int32_t cap) {
    return llama_tokenize(llama_model_get_vocab(static_cast<const llama_model *>(model)), text, len, out, cap, false, true);
}

// `kv_type`: 0 f16, 1 q8_0. `flash`: 1 on. `n_rs_seq`: recurrent-state snapshots per sequence, so
// the last tokens can be removed again (a rejected draft). `mtp`: 1 makes the MTP draft context
// (only the model's NextN layer and its own small KV cache).
void * bn_context_new(void * model, uint32_t n_ctx, uint32_t n_batch, uint32_t n_ubatch, uint32_t n_seq_max, int32_t kv_type,
                      int32_t flash, int32_t threads, uint32_t n_rs_seq, int32_t mtp, char * error, size_t error_len) {
    auto params = llama_context_default_params();
    params.n_rs_seq = n_rs_seq;
    if (mtp) {
        params.ctx_type = LLAMA_CONTEXT_TYPE_MTP;
    }
    params.n_ctx = n_ctx;
    params.n_batch = n_batch;
    params.n_ubatch = n_ubatch;
    params.n_seq_max = n_seq_max;
    params.kv_unified = true;
    params.flash_attn_type = flash ? LLAMA_FLASH_ATTN_TYPE_ENABLED : LLAMA_FLASH_ATTN_TYPE_AUTO;
    params.type_k = kv_type == 1 ? GGML_TYPE_Q8_0 : GGML_TYPE_F16;
    params.type_v = kv_type == 1 ? GGML_TYPE_Q8_0 : GGML_TYPE_F16;
    if (threads > 0) {
        params.n_threads = threads;
        params.n_threads_batch = threads;
    }
    params.no_perf = true;
    llama_context * ctx = llama_init_from_model(static_cast<llama_model *>(model), params);
    if (!ctx) {
        set_error(error, error_len, "llama.cpp could not create the context (see the plugin log)");
    }
    return ctx;
}

void bn_context_free(void * ctx) {
    llama_free(static_cast<llama_context *>(ctx));
}

uint32_t bn_n_ctx(const void * ctx) {
    return llama_n_ctx(static_cast<const llama_context *>(ctx));
}

// Decode `n` tokens of one sequence at positions `pos0..`; `logits`: 0 none, 1 the last token's,
// 2 every token's. Returns llama_decode's status (0 ok). `n` must not exceed the context's n_batch.
int32_t bn_decode(void * ctx, const int32_t * tokens, int32_t n, int32_t pos0, int32_t seq, int32_t logits) {
    llama_batch batch = llama_batch_init(n, 0, 1);
    for (int32_t i = 0; i < n; i++) {
        batch.token[i] = tokens[i];
        batch.pos[i] = pos0 + i;
        batch.n_seq_id[i] = 1;
        batch.seq_id[i][0] = seq;
        batch.logits[i] = logits == 2 || (logits == 1 && i == n - 1);
    }
    batch.n_tokens = n;
    int32_t status = llama_decode(static_cast<llama_context *>(ctx), batch);
    llama_batch_free(batch);
    return status;
}

// An MTP batch: each row is a token and the target's hidden state `embd` (n rows of `n_embd`
// floats) at the position before it; logits only for the last row when `last_logits`.
int32_t bn_decode_embd(void * ctx, const int32_t * tokens, const float * embd, int32_t n, int32_t n_embd, int32_t pos0, int32_t seq,
                       int32_t last_logits) {
    llama_batch batch = llama_batch_init(n, n_embd, 1);
    // llama_batch_init allocates either tokens or rows; the MTP head reads both (freed by llama_batch_free).
    batch.token = static_cast<llama_token *>(malloc(sizeof(llama_token) * n));
    for (int32_t i = 0; i < n; i++) {
        batch.token[i] = tokens[i];
        batch.pos[i] = pos0 + i;
        batch.n_seq_id[i] = 1;
        batch.seq_id[i][0] = seq;
        batch.logits[i] = last_logits && i == n - 1;
    }
    memcpy(batch.embd, embd, sizeof(float) * (size_t) n * (size_t) n_embd);
    batch.n_tokens = n;
    int32_t status = llama_decode(static_cast<llama_context *>(ctx), batch);
    llama_batch_free(batch);
    return status;
}

// An image's rows: `n` embeddings of `n_embd` floats with their positions, `pos4` holding (t, x, y,
// z) per row (absolute). `mrope` 1: the model reads four positions per row (Qwen's M-RoPE), else t.
int32_t bn_decode_image(void * ctx, const float * embd, int32_t n, int32_t n_embd, const int32_t * pos4, int32_t mrope, int32_t seq) {
    const int32_t per_row = mrope ? 4 : 1;
    std::vector<llama_pos> pos((size_t) n * per_row);
    for (int32_t i = 0; i < n; i++) {
        // Planar, one section after another: t, y, x, z (mtmd-helper's set_position_mrope_2d).
        pos[i] = pos4[i * 4];
        if (mrope) {
            pos[i + n] = pos4[i * 4 + 2];
            pos[i + n * 2] = pos4[i * 4 + 1];
            pos[i + n * 3] = pos4[i * 4 + 3];
        }
    }
    std::vector<int32_t> n_seq_id(n, 1);
    llama_seq_id seq_id = seq;
    std::vector<llama_seq_id *> seq_ids(n + 1, &seq_id);
    seq_ids[n] = nullptr;
    std::vector<int8_t> logits(n, 0);
    llama_batch batch = {
        /*n_tokens =*/ n,
        /*token    =*/ nullptr,
        /*embd     =*/ const_cast<float *>(embd),
        /*pos      =*/ pos.data(),
        /*n_seq_id =*/ n_seq_id.data(),
        /*seq_id   =*/ seq_ids.data(),
        /*logits   =*/ logits.data(),
    };
    (void) n_embd;
    return llama_decode(static_cast<llama_context *>(ctx), batch);
}

// Floats per input embedding row (what an image row must have).
int32_t bn_model_n_embd_inp(const void * model) {
    return llama_model_n_embd_inp(static_cast<const llama_model *>(model));
}

// ---- images (mtmd) ------------------------------------------------------------------------------

// The vision projector for `model`; `max_tokens` caps an image's tokens (larger images are
// downscaled), `use_gpu` 0 runs the encoder on the CPU.
void * bn_vision_load(const char * path, const void * model, int32_t use_gpu, int32_t threads, int32_t max_tokens, char * error, size_t error_len) {
    mtmd_log_set(log_callback, nullptr);
    auto params = mtmd_context_params_default();
    params.use_gpu = use_gpu != 0;
    params.print_timings = false;
    params.warmup = false;
    if (threads > 0) {
        params.n_threads = threads;
    }
    if (max_tokens > 0) {
        params.image_max_tokens = max_tokens;
    }
    mtmd_context * ctx = mtmd_init_from_file(path, static_cast<const llama_model *>(model), params);
    if (!ctx) {
        set_error(error, error_len, std::string("llama.cpp could not load the vision projector ") + path + " (see the plugin log)");
    } else if (!mtmd_support_vision(ctx)) {
        mtmd_free(ctx);
        ctx = nullptr;
        set_error(error, error_len, std::string(path) + " has no vision encoder");
    }
    return ctx;
}

void bn_vision_free(void * vision) {
    mtmd_free(static_cast<mtmd_context *>(vision));
}

int32_t bn_vision_mrope(const void * vision) {
    return mtmd_decode_use_mrope(static_cast<const mtmd_context *>(vision)) ? 1 : 0;
}

// Preprocess an RGB image (`nx` × `ny`, 3 bytes per pixel) into its chunk: returns the chunk (free
// it with bn_vision_chunk_free) and its token and position counts.
void * bn_vision_prepare(void * vision, const uint8_t * rgb, uint32_t nx, uint32_t ny, int32_t * n_tokens, int32_t * n_pos, char * error, size_t error_len) {
    auto * ctx = static_cast<mtmd_context *>(vision);
    mtmd_bitmap * bitmap = mtmd_bitmap_init(nx, ny, rgb);
    if (!bitmap) {
        set_error(error, error_len, "could not take the image");
        return nullptr;
    }
    mtmd_input_chunks * chunks = mtmd_input_chunks_init();
    const char * marker = mtmd_default_marker();
    mtmd_input_text text = {marker, strlen(marker), false, true};
    const mtmd_bitmap * bitmaps[] = {bitmap};
    int32_t status = mtmd_tokenize(ctx, chunks, &text, bitmaps, 1);
    mtmd_input_chunk * image = nullptr;
    if (status == 0) {
        for (size_t i = 0; i < mtmd_input_chunks_size(chunks); i++) {
            const mtmd_input_chunk * chunk = mtmd_input_chunks_get(chunks, i);
            if (mtmd_input_chunk_get_type(chunk) == MTMD_INPUT_CHUNK_TYPE_IMAGE) {
                image = mtmd_input_chunk_copy(chunk);
                break;
            }
        }
    }
    mtmd_input_chunks_free(chunks);
    mtmd_bitmap_free(bitmap);
    if (!image) {
        set_error(error, error_len, status == 2 ? "the image could not be preprocessed" : "the image gave no image tokens");
        return nullptr;
    }
    *n_tokens = (int32_t) mtmd_input_chunk_get_n_tokens(image);
    *n_pos = mtmd_input_chunk_get_n_pos(image);
    return image;
}

// Each row's position relative to the image's first (t, x, y, z per row).
void bn_vision_positions(const void * chunk, int32_t * out) {
    const auto * image = mtmd_input_chunk_get_tokens_image(static_cast<const mtmd_input_chunk *>(chunk));
    size_t n = mtmd_image_tokens_get_n_tokens(image);
    for (size_t i = 0; i < n; i++) {
        mtmd_decoder_pos pos = mtmd_image_tokens_get_decoder_pos(image, 0, i);
        out[i * 4] = (int32_t) pos.t;
        out[i * 4 + 1] = (int32_t) pos.x;
        out[i * 4 + 2] = (int32_t) pos.y;
        out[i * 4 + 3] = (int32_t) pos.z;
    }
}

// Run the encoder on a chunk; its embeddings are then at bn_vision_embd (n_tokens × n_embd_inp).
int32_t bn_vision_encode(void * vision, const void * chunk) {
    return mtmd_encode_chunk(static_cast<mtmd_context *>(vision), static_cast<const mtmd_input_chunk *>(chunk));
}

const float * bn_vision_embd(void * vision) {
    return mtmd_get_output_embd(static_cast<mtmd_context *>(vision));
}

void bn_vision_chunk_free(void * chunk) {
    mtmd_input_chunk_free(static_cast<mtmd_input_chunk *>(chunk));
}

// The logits of the last token that requested them (n_vocab floats).
const float * bn_logits_last(void * ctx) {
    return llama_get_logits_ith(static_cast<llama_context *>(ctx), -1);
}

// The logits of row `i` of the last batch (it must have requested them).
const float * bn_logits_ith(void * ctx, int32_t i) {
    return llama_get_logits_ith(static_cast<llama_context *>(ctx), i);
}

// Keep the hidden state the MTP head reads: for every row of each batch (`masked` 0, the target),
// or only for rows with logits (`masked` 1, the draft context).
void bn_set_embeddings_nextn(void * ctx, int32_t value, int32_t masked) {
    llama_set_embeddings_nextn(static_cast<llama_context *>(ctx), value != 0, masked != 0);
}

// The hidden state of row `i` of the last batch (n_embd_out floats), null when there is none.
const float * bn_embeddings_nextn_ith(void * ctx, int32_t i) {
    return llama_get_embeddings_nextn_ith(static_cast<llama_context *>(ctx), i);
}

void bn_synchronize(void * ctx) {
    llama_synchronize(static_cast<llama_context *>(ctx));
}

int32_t bn_seq_rm(void * ctx, int32_t seq, int32_t p0, int32_t p1) {
    return llama_memory_seq_rm(llama_get_memory(static_cast<llama_context *>(ctx)), seq, p0, p1) ? 1 : 0;
}

void bn_seq_cp(void * ctx, int32_t from, int32_t to) {
    llama_memory_seq_cp(llama_get_memory(static_cast<llama_context *>(ctx)), from, to, -1, -1);
}

int32_t bn_seq_pos_max(void * ctx, int32_t seq) {
    return llama_memory_seq_pos_max(llama_get_memory(static_cast<llama_context *>(ctx)), seq);
}

// Sequence state: `partial` 1 = the recurrent part only (a context checkpoint), 0 = everything.
uint64_t bn_state_size(void * ctx, int32_t seq, int32_t partial) {
    return llama_state_seq_get_size_ext(static_cast<llama_context *>(ctx), seq, partial ? LLAMA_STATE_SEQ_FLAGS_PARTIAL_ONLY : 0);
}

uint64_t bn_state_get(void * ctx, uint8_t * dst, uint64_t size, int32_t seq, int32_t partial) {
    return llama_state_seq_get_data_ext(static_cast<llama_context *>(ctx), dst, size, seq, partial ? LLAMA_STATE_SEQ_FLAGS_PARTIAL_ONLY : 0);
}

uint64_t bn_state_set(void * ctx, const uint8_t * src, uint64_t size, int32_t seq, int32_t partial) {
    return llama_state_seq_set_data_ext(static_cast<llama_context *>(ctx), src, size, seq, partial ? LLAMA_STATE_SEQ_FLAGS_PARTIAL_ONLY : 0);
}

// The GPU's memory as Metal reports it: `total` = recommendedMaxWorkingSetSize, `free` = total less
// currentAllocatedSize (this process's allocations). Returns 0 when there is no GPU device.
int32_t bn_gpu_memory(uint64_t * free, uint64_t * total, char * name, size_t len) {
    ggml_backend_dev_t dev = ggml_backend_dev_by_type(GGML_BACKEND_DEVICE_TYPE_GPU);
    if (!dev) {
        return 0;
    }
    size_t f = 0;
    size_t t = 0;
    ggml_backend_dev_memory(dev, &f, &t);
    *free = f;
    *total = t;
    if (name && len > 0) {
        snprintf(name, len, "%s", ggml_backend_dev_description(dev));
    }
    return 1;
}

}  // extern "C"
