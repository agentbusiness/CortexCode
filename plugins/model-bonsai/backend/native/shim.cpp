// The flat C ABI the Rust engine calls (src/llama.rs). Every llama.cpp struct is built and read
// here, so Rust passes and receives only pointers, integers and floats. One context is used by one
// thread at a time (the engine thread); the shim adds no locking.
#include "ggml-backend.h"
#include "llama.h"

#include <cstdarg>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>

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

void * bn_model_load(const char * path, int32_t n_gpu_layers, char * error, size_t error_len) {
    auto params = llama_model_default_params();
    params.n_gpu_layers = n_gpu_layers;
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

// `kv_type`: 0 f16, 1 q8_0. `flash`: 1 on.
void * bn_context_new(void * model, uint32_t n_ctx, uint32_t n_batch, uint32_t n_ubatch, uint32_t n_seq_max, int32_t kv_type,
                      int32_t flash, int32_t threads, char * error, size_t error_len) {
    auto params = llama_context_default_params();
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

// Decode `n` tokens of one sequence at positions `pos0..`; logits only for the last token when
// `last_logits`. Returns llama_decode's status (0 ok). `n` must not exceed the context's n_batch.
int32_t bn_decode(void * ctx, const int32_t * tokens, int32_t n, int32_t pos0, int32_t seq, int32_t last_logits) {
    llama_batch batch = llama_batch_init(n, 0, 1);
    for (int32_t i = 0; i < n; i++) {
        batch.token[i] = tokens[i];
        batch.pos[i] = pos0 + i;
        batch.n_seq_id[i] = 1;
        batch.seq_id[i][0] = seq;
        batch.logits[i] = last_logits && i == n - 1;
    }
    batch.n_tokens = n;
    int32_t status = llama_decode(static_cast<llama_context *>(ctx), batch);
    llama_batch_free(batch);
    return status;
}

// The logits of the last token that requested them (n_vocab floats).
const float * bn_logits_last(void * ctx) {
    return llama_get_logits_ith(static_cast<llama_context *>(ctx), -1);
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
