//! Every model call this process made, one row each: which model answered, for which tag, on which
//! computer, and its tokens (in, cached, out and reasoning) and times.
//!
//! The rows come from the calls the process makes (the plugin runtime's outgoing observer):
//! `mesh.claim` answers where the key's calls go (its `Placement`), and each `model.chat` or
//! `model.complete` with that key as its `cache_key` is a row with that placement. A call without a
//! claim ran on this app's own model.

use crate::{now_ms, recorder};
use cc_protocol::RpcError;
use cc_protocol::conn::Watch;
use cc_protocol::contracts::mesh::methods as mesh_methods;
use cc_protocol::contracts::model::methods as model_methods;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Rows held between two hand-overs; a call past this many is not recorded.
const MAX_ROWS: usize = 4096;
/// Placements held; the oldest goes first.
const MAX_PLACEMENTS: usize = 1024;

/// One model call.
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
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub prefill_ms: f64,
    pub decode_ms: f64,
    pub queue_ms: f64,
    /// From the call to its answer, as this process saw it.
    pub wall_ms: f64,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
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

static ROWS: Mutex<Vec<ModelRecord>> = Mutex::new(Vec::new());
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
    let (part, version, node) = recorder().map(|r| (r.part.clone(), r.version.clone(), r.node.clone())).unwrap_or_default();
    let row = ModelRecord {
        schema_version: 1,
        event_id: uuid::Uuid::new_v4().to_string(),
        part,
        method,
        tag: place.tag,
        model_id: place.model_id,
        plugin: place.plugin,
        served_by: place.name,
        served_node: place.node,
        tier: place.tier,
        outcome: code,
        finish,
        prompt_tokens: count("prompt_tokens"),
        cached_tokens: count("cached_tokens"),
        completion_tokens: count("completion_tokens"),
        reasoning_tokens: count("reasoning_tokens"),
        prefill_ms: ms("prefill_ms"),
        decode_ms: ms("decode_ms"),
        queue_ms: ms("queue_ms"),
        wall_ms,
        started_at_ms: started,
        ended_at_ms: now_ms(),
        version,
        node_id: node,
    };
    if let Ok(mut rows) = ROWS.lock()
        && rows.len() < MAX_ROWS
    {
        rows.push(row);
    }
}

/// Takes every row.
pub(crate) fn drain() -> Vec<Value> {
    let taken = match ROWS.lock() {
        Ok(mut rows) => std::mem::take(&mut *rows),
        Err(_) => return Vec::new(),
    };
    taken.iter().filter_map(|record| serde_json::to_value(record).ok()).collect()
}
