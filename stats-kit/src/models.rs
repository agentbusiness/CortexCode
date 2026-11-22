//! The model calls this process made, summed per five-minute bucket
//! ([`SUMMARY_MS`](cc_protocol::contracts::reports::SUMMARY_MS)): which model answered, for which
//! tag, on which computer, how it ended, how many calls, and their tokens (in, cached, out and
//! reasoning) and times.
//!
//! The calls come from the plugin runtime's outgoing observer: `mesh.claim` answers where the
//! key's calls go (its `Placement`), and each `model.chat` or `model.complete` with that key as its
//! `cache_key` adds to the row of that placement. A call without a claim ran on this app's own
//! model.

use crate::{bucket_of, now_ms, recorder};
use cc_protocol::contracts::reports::SUMMARY_MS;
use cc_protocol::RpcError;
use cc_protocol::conn::Watch;
use cc_protocol::contracts::mesh::methods as mesh_methods;
use cc_protocol::contracts::model::methods as model_methods;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Rows held between two hand-overs; a call that would open one more is not recorded.
const MAX_ROWS: usize = 4096;
/// Placements held; the oldest goes first.
const MAX_PLACEMENTS: usize = 1024;

/// The calls of one model, tag, computer and outcome within one five-minute bucket.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelRecord {
    pub schema_version: u32,
    pub event_id: String,
    /// The process that made it.
    pub part: String,
    /// `model.chat` or `model.complete`.
    pub method: String,
    /// The tag the model was claimed for (`coding`, `office`); empty: not claimed.
    pub tag: String,
    /// The model that answered (its id on its computer's card); empty: this app's own.
    pub model_id: String,
    /// The model plugin on that computer.
    pub plugin: String,
    /// The computer that served it, as its card names it; empty: this one.
    pub served_by: String,
    /// That computer's node (hex); empty: this one.
    pub served_node: String,
    /// `local` | `machine` | `lan` | `remote`; empty: not claimed (this app's own model).
    pub tier: String,
    /// `ok`, or the error's code.
    pub outcome: String,
    /// The model's `finish_reason` (`stop`, `length`, `tool_calls`, ...).
    pub finish: String,
    /// The calls summed in this row.
    #[serde(default)]
    pub count: u64,
    /// Summed over the row's calls.
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    /// Summed over the row's calls.
    pub prefill_ms: f64,
    pub decode_ms: f64,
    pub queue_ms: f64,
    /// From the call to its answer, as this process saw it, summed over the row's calls.
    pub wall_ms: f64,
    /// The longest of them.
    #[serde(default)]
    pub max_wall_ms: f64,
    /// The first call's start and the last call's end.
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    /// The bucket's start (a multiple of `window_ms`, UTC) and its length.
    #[serde(default)]
    pub bucket_ms: i64,
    #[serde(default)]
    pub window_ms: i64,
    pub version: String,
    pub node_id: String,
}

#[derive(Clone, Default)]
struct Placed {
    tag: String,
    model_id: String,
    plugin: String,
    name: String,
    node: String,
    tier: String,
}

/// Bucket, method, tag, model, plugin, computer name, computer node, tier, outcome, finish.
type Key = (i64, String, String, String, String, String, String, String, String, String);

static ROWS: OnceLock<Mutex<HashMap<Key, ModelRecord>>> = OnceLock::new();

fn rows() -> &'static Mutex<HashMap<Key, ModelRecord>> {
    ROWS.get_or_init(|| Mutex::new(HashMap::new()))
}
static PLACED: OnceLock<Mutex<(HashMap<String, Placed>, Vec<String>)>> = OnceLock::new();

fn placed() -> &'static Mutex<(HashMap<String, Placed>, Vec<String>)> {
    PLACED.get_or_init(|| Mutex::new((HashMap::new(), Vec::new())))
}

fn text(value: &Value, name: &str) -> String {
    value.get(name).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// The plugin runtime's outgoing observer: a claim's placement is kept for its key, a model call is
/// a row.
pub fn observe(method: &str, params: &Value) -> Option<Watch> {
    match method {
        mesh_methods::CLAIM => {
            if text(params, "kind") != "model" {
                return None;
            }
            let key = text(params, "session_id");
            let tag = params.get("tags").and_then(Value::as_array).and_then(|tags| tags.first()).and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| text(params, "tag"));
            Some(Box::new(move |outcome: &Result<Value, RpcError>| {
                let Ok(placement) = outcome else { return };
                if key.is_empty() || text(placement, "tier") == "busy" {
                    return;
                }
                let place = Placed {
                    tag,
                    model_id: text(placement, "model_id"),
                    plugin: text(placement, "plugin"),
                    name: text(placement, "name"),
                    node: text(placement, "node"),
                    tier: text(placement, "tier"),
                };
                let Ok(mut held) = placed().lock() else { return };
                let (map, order) = &mut *held;
                if map.insert(key.clone(), place).is_none() {
                    order.push(key);
                }
                if order.len() > MAX_PLACEMENTS {
                    let gone: Vec<String> = order.drain(..order.len() - MAX_PLACEMENTS).collect();
                    for key in gone {
                        map.remove(&key);
                    }
                }
            }))
        }
        model_methods::CHAT | model_methods::COMPLETE => {
            let key = text(params, "cache_key");
            let method = method.to_owned();
            let started = now_ms();
            let clock = std::time::Instant::now();
            Some(Box::new(move |outcome: &Result<Value, RpcError>| record(method, &key, started, clock.elapsed().as_secs_f64() * 1000.0, outcome)))
        }
        _ => None,
    }
}

fn record(method: String, key: &str, started: i64, wall_ms: f64, outcome: &Result<Value, RpcError>) {
    let place = if key.is_empty() { None } else { placed().lock().ok().and_then(|held| held.0.get(key).cloned()) };
    let place = place.unwrap_or_default();
    let (code, finish, usage) = match outcome {
        Ok(value) => ("ok".to_owned(), text(value, "finish_reason"), value.get("usage").cloned().unwrap_or(Value::Null)),
        Err(error) => (error.code.to_string(), String::new(), Value::Null),
    };
    let count = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    let ms = |name: &str| usage.get(name).and_then(Value::as_f64).unwrap_or(0.0);
    let ended = now_ms();
    let key: Key = (bucket_of(ended), method, place.tag, place.model_id, place.plugin, place.name, place.node, place.tier, code, finish);
    let Ok(mut rows) = rows().lock() else { return };
    if let Some(row) = rows.get_mut(&key) {
        row.count += 1;
        row.prompt_tokens += count("prompt_tokens");
        row.cached_tokens += count("cached_tokens");
        row.completion_tokens += count("completion_tokens");
        row.reasoning_tokens += count("reasoning_tokens");
        row.prefill_ms += ms("prefill_ms");
        row.decode_ms += ms("decode_ms");
        row.queue_ms += ms("queue_ms");
        row.wall_ms += wall_ms;
        row.max_wall_ms = row.max_wall_ms.max(wall_ms);
        row.started_at_ms = row.started_at_ms.min(started);
        row.ended_at_ms = row.ended_at_ms.max(ended);
        return;
    }
    if rows.len() >= MAX_ROWS {
        return;
    }
    let (part, version, node) = recorder().map(|r| (r.part.clone(), r.version.clone(), r.node.clone())).unwrap_or_default();
    let row = ModelRecord {
        schema_version: 2,
        event_id: uuid::Uuid::new_v4().to_string(),
        part,
        method: key.1.clone(),
        tag: key.2.clone(),
        model_id: key.3.clone(),
        plugin: key.4.clone(),
        served_by: key.5.clone(),
        served_node: key.6.clone(),
        tier: key.7.clone(),
        outcome: key.8.clone(),
        finish: key.9.clone(),
        count: 1,
        prompt_tokens: count("prompt_tokens"),
        cached_tokens: count("cached_tokens"),
        completion_tokens: count("completion_tokens"),
        reasoning_tokens: count("reasoning_tokens"),
        prefill_ms: ms("prefill_ms"),
        decode_ms: ms("decode_ms"),
        queue_ms: ms("queue_ms"),
        wall_ms,
        max_wall_ms: wall_ms,
        started_at_ms: started,
        ended_at_ms: ended,
        bucket_ms: key.0,
        window_ms: SUMMARY_MS,
        version,
        node_id: node,
    };
    rows.insert(key, row);
}

/// Takes the rows of the closed buckets (with `open_too`, every row).
pub(crate) fn drain(open_too: bool) -> Vec<Value> {
    let open = bucket_of(now_ms());
    let taken: Vec<ModelRecord> = match rows().lock() {
        Ok(mut rows) => {
            let keys: Vec<Key> = rows.keys().filter(|key| open_too || key.0 < open).cloned().collect();
            keys.iter().filter_map(|key| rows.remove(key)).collect()
        }
        Err(_) => return Vec::new(),
    };
    taken.iter().filter_map(|record| serde_json::to_value(record).ok()).collect()
}
