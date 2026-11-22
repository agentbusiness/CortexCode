//! The contract methods this process serves, summed per five-minute bucket
//! ([`SUMMARY_MS`](cc_protocol::contracts::reports::SUMMARY_MS)): how many, how long, how they
//! ended, and for a model's answer its tokens (`usage` of the model contract's result). The kit's
//! own hand-over (`stats.record`) is not counted: it would only measure the measuring.

use crate::{bucket_of, now_ms, recorder};
use cc_protocol::RpcError;
use cc_protocol::contracts::reports::{SUMMARY_MS, methods};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Groups held between two hand-overs; a call past this many is not counted.
const MAX_GROUPS: usize = 4096;

/// One method with one outcome within one five-minute bucket.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CallRecord {
    pub schema_version: u32,
    pub event_id: String,
    /// The process that served it.
    pub part: String,
    pub method: String,
    /// `ok`, or the error's code.
    pub outcome: String,
    pub count: u64,
    pub total_us: u64,
    pub max_us: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    /// The first and the last call of the bucket.
    pub first_at_ms: i64,
    pub last_at_ms: i64,
    /// The bucket's start (a multiple of `window_ms`, UTC) and its length.
    #[serde(default)]
    pub bucket_ms: i64,
    #[serde(default)]
    pub window_ms: i64,
    pub version: String,
    pub node_id: String,
}

type Key = (i64, String, String);

static GROUPS: OnceLock<Mutex<HashMap<Key, CallRecord>>> = OnceLock::new();

fn groups() -> &'static Mutex<HashMap<Key, CallRecord>> {
    GROUPS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tokens(result: &Value, name: &str) -> u64 {
    result.get("usage").and_then(|usage| usage.get(name)).and_then(Value::as_u64).unwrap_or(0)
}

/// Count one served call (the plugin runtime's observer).
pub fn observe(method: &str, took: Duration, outcome: &Result<Value, RpcError>) {
    if method == methods::RECORD {
        return;
    }
    let now = now_ms();
    let us = took.as_micros() as u64;
    let (code, prompt, cached, completion) = match outcome {
        Ok(value) => ("ok".to_string(), tokens(value, "prompt_tokens"), tokens(value, "cached_tokens"), tokens(value, "completion_tokens")),
        Err(error) => (error.code.to_string(), 0, 0, 0),
    };
    let Ok(mut table) = groups().lock() else { return };
    let key = (bucket_of(now), method.to_owned(), code);
    if let Some(group) = table.get_mut(&key) {
        group.count += 1;
        group.total_us = group.total_us.saturating_add(us);
        group.max_us = group.max_us.max(us);
        group.prompt_tokens += prompt;
        group.cached_tokens += cached;
        group.completion_tokens += completion;
        group.last_at_ms = now;
        return;
    }
    if table.len() >= MAX_GROUPS {
        return;
    }
    let (part, version, node) = recorder().map(|r| (r.part.clone(), r.version.clone(), r.node.clone())).unwrap_or_default();
    table.insert(
        key.clone(),
        CallRecord {
            schema_version: 2,
            event_id: uuid::Uuid::new_v4().to_string(),
            part,
            method: key.1.clone(),
            outcome: key.2.clone(),
            count: 1,
            total_us: us,
            max_us: us,
            prompt_tokens: prompt,
            cached_tokens: cached,
            completion_tokens: completion,
            first_at_ms: now,
            last_at_ms: now,
            bucket_ms: key.0,
            window_ms: SUMMARY_MS,
            version,
            node_id: node,
        },
    );
}

/// Takes the groups of the closed buckets (with `open_too`, every group), as rows.
pub(crate) fn drain(open_too: bool) -> Vec<Value> {
    let open = bucket_of(now_ms());
    let taken: Vec<CallRecord> = match groups().lock() {
        Ok(mut table) => {
            let keys: Vec<Key> = table.keys().filter(|key| open_too || key.0 < open).cloned().collect();
            keys.iter().filter_map(|key| table.remove(key)).collect()
        }
        Err(_) => return Vec::new(),
    };
    taken.iter().filter_map(|record| serde_json::to_value(record).ok()).collect()
}
