//! What this plugin keeps, in the app's graph (`memory@1`):
//!
//! - setting `model.variant` (global scope): the model the user chose (`model.select`);
//! - `(:Model {key: model id, variant, kv_type, kv_ok*})-[:ON]->(:Machine)` (OBSERVED layer): what
//!   this machine learned about the model: whether llama-server runs it with the quantized KV cache
//!   `kv_type` (flash attention), checked once; `kv_ok` keeps its history.
//!
//! The model must serve while the memory plugin is still starting (or absent): reads wait a few
//! seconds at most and the plugin goes on with its defaults; writes are tried again in the
//! background until the memory plugin takes them. Weights, their checksums, the llama-server pid
//! and log stay files (plumbing).

use cc_protocol::contracts::memory::methods;
use cc_protocol::contracts::records::methods as records;
use cc_protocol::{Ctx, RpcError, codes};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// The chosen model.
pub const VARIANT_SETTING: &str = "model.variant";
/// Between two attempts while the memory plugin does not answer.
const RETRY: Duration = Duration::from_secs(1);

fn waiting(error: &RpcError) -> bool {
    matches!(error.code.as_str(), codes::UNAVAILABLE | codes::CONNECTION_CLOSED | codes::DRAINING | codes::TIMEOUT)
}

/// A call, waiting up to `bound` (None: as long as it takes) for the memory plugin to answer.
async fn call(ctx: &Ctx, method: &str, params: Value, bound: Option<Duration>) -> Result<Value, RpcError> {
    let started = Instant::now();
    loop {
        match ctx.call(method, params.clone()).await {
            Err(error) if waiting(&error) && bound.is_none_or(|bound| started.elapsed() < bound) => tokio::time::sleep(RETRY).await,
            other => return other,
        }
    }
}

/// The chosen model. `Err` when the memory plugin did not answer within `bound`.
pub async fn variant(ctx: &Ctx, bound: Duration) -> Result<Option<String>, ()> {
    match call(ctx, records::SETTINGS_GET, json!({ "path": VARIANT_SETTING }), Some(bound)).await {
        Ok(setting) => Ok(setting.get("value").and_then(Value::as_str).map(str::to_owned)),
        Err(error) if error.code == codes::NOT_FOUND => Ok(None),
        Err(error) => {
            tracing::info!(error = %error.message, "the chosen model was not read from the settings yet");
            Err(())
        }
    }
}

/// Keep the chosen model (in the background, until the memory plugin takes it).
pub fn save_variant(ctx: &Ctx, id: &str) {
    let (ctx, id) = (ctx.clone(), id.to_owned());
    tokio::spawn(async move {
        if let Err(error) = call(&ctx, records::SETTINGS_SET, json!({ "path": VARIANT_SETTING, "value": id, "by": "model" }), None).await {
            tracing::warn!(error = %error.message, "the chosen model was not kept");
        }
    });
}

/// What this machine learned about the quantized KV cache for `model_id`: `(kv_type, ok)`. `None`
/// when nothing is known or the memory plugin did not answer within `bound`.
pub async fn kv_check(ctx: &Ctx, model_id: &str, bound: Duration) -> Option<(String, bool)> {
    let query = json!({ "cypher": "MATCH (m:Model {key: $key})-[:ON]->(:Machine) RETURN m.kv_type, m.kv_ok", "params": { "key": model_id } });
    let rows = call(ctx, methods::GRAPH_QUERY, query, Some(bound)).await.ok()?;
    let row = rows.get("rows")?.as_array()?.first()?.as_array()?.clone();
    Some((row.first()?.as_str()?.to_owned(), row.get(1)?.as_bool()?))
}

/// Record a KV cache check (in the background, until the memory plugin takes it).
pub fn save_kv(ctx: &Ctx, model_id: &str, variant: &str, kv_type: &str, ok: bool) {
    let (ctx, model_id, variant, kv_type) = (ctx.clone(), model_id.to_owned(), variant.to_owned(), kv_type.to_owned());
    tokio::spawn(async move {
        if let Err(error) = put_kv(&ctx, &model_id, &variant, &kv_type, ok).await {
            tracing::warn!(error = %error.message, "the KV cache check was not recorded");
        }
    });
}

async fn put_kv(ctx: &Ctx, model_id: &str, variant: &str, kv_type: &str, ok: bool) -> Result<(), RpcError> {
    let machine = call(ctx, methods::GRAPH_MACHINE, json!({}), None).await?;
    let machine = machine.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
    let put = json!({
        "nodes": [{ "label": "Model", "key": model_id, "props": { "variant": variant, "kv_type": kv_type }, "temporal": { "kv_ok": ok } }],
        "edges": [{ "from": { "label": "Model", "key": model_id }, "rel": "ON", "to": { "label": "Machine", "key": machine } }],
        "layer": "observed",
    });
    call(ctx, methods::GRAPH_PUT, put, None).await.map(|_| ())
}
