//! The chat flow's records in the memory plugin's graph (`docs/GRAPH-MODEL.md`), all in the
//! workspace layer. Session journals are the one exception: they stay files.
//!
//! ```text
//! (:Snapshot {key: '<session>/<run>', version, root, head, subdir, untracked, dir, created_ms})-[:OF]->(:Session)
//! (:Replay {key: '<lesson>#<ts_ms>', ts_ms, lesson, name, text, dom, with_state, without_state, verdict, evidence, of})-[:FOR]->(:Session)
//! (:NightReport {key: <date>, text, created_ms, lessons, keyed, repair, skill, replays, failing, topic_only})-[:ON]->(:Machine)
//! (:Attachment {key: sha256, name, mime, bytes, ext, path, kind, width?, height?})-[:IN]->(:Session)
//! stream `replays`: every replay's result, as it was decided
//! ```
//!
//! A record whose session is not in the graph (deleted, or never written there) hangs `ON` the
//! `Machine` instead, so nothing is kept unconnected.

use crate::host::Host;
use cc_protocol::contracts::memory::{GraphEdge, GraphLayer, GraphNode, GraphPutParams, GraphQueryResult, GraphRef, GraphRetireParams, MachineInfo, StreamAppendParams, methods};
use cc_protocol::{RpcError, codes};
use serde_json::{Map, Value, json};
use tokio::sync::OnceCell;

/// This Mac's root node's key (asked once).
static MACHINE: OnceCell<String> = OnceCell::const_new();

pub async fn machine(host: &Host) -> Result<String, RpcError> {
    MACHINE.get_or_try_init(|| async { host.get::<MachineInfo>(methods::GRAPH_MACHINE, json!({})).await.map(|info| info.key) }).await.cloned()
}

pub fn node(label: &str, key: &str, props: Map<String, Value>) -> GraphNode {
    GraphNode { label: label.to_owned(), key: key.to_owned(), props, temporal: Map::new() }
}

fn edge(from: &GraphNode, rel: &str, label: &str, key: &str) -> GraphEdge {
    GraphEdge { from: GraphRef { label: from.label.clone(), key: from.key.clone() }, rel: rel.to_owned(), to: GraphRef { label: label.to_owned(), key: key.to_owned() }, props: Map::new(), temporal: Map::new() }
}

/// The props of a `json!` object (anything else is empty).
pub fn props(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

async fn put(host: &Host, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>, at_ms: Option<u64>) -> Result<(), RpcError> {
    let params = GraphPutParams { nodes, edges, layer: GraphLayer::Workspace, at_ms };
    host.call(methods::GRAPH_PUT, serde_json::to_value(params).map_err(|error| RpcError::internal(error.to_string()))?).await.map(|_| ())
}

/// Put `node` on this Mac's root (`-[:ON]->(:Machine)`).
pub async fn put_on_machine(host: &Host, node: GraphNode, at_ms: Option<u64>) -> Result<(), RpcError> {
    let machine = machine(host).await?;
    let on = edge(&node, "ON", "Machine", &machine);
    put(host, vec![self::node("Machine", &machine, Map::new()), node], vec![on], at_ms).await
}

/// Put `node` with an edge `rel` to its session; on the Machine when the session is not in the
/// graph (the put names an edge end that is not stored).
pub async fn put_in_session(host: &Host, node: GraphNode, rel: &str, session_id: Option<&str>, at_ms: Option<u64>) -> Result<(), RpcError> {
    if let Some(session_id) = session_id.filter(|id| !id.is_empty()) {
        let to = edge(&node, rel, "Session", session_id);
        match put(host, vec![node.clone()], vec![to], at_ms).await {
            Ok(()) => return Ok(()),
            Err(error) if error.code == codes::INVALID_PARAMS => tracing::debug!(error = %error.message, session = %session_id, "the session is not in the graph; the record goes on the machine"),
            Err(error) => return Err(error),
        }
    }
    put_on_machine(host, node, at_ms).await
}

pub async fn retire(host: &Host, label: &str, keys: Vec<String>, reason: &str) -> Result<(), RpcError> {
    if keys.is_empty() {
        return Ok(());
    }
    let refs = keys.into_iter().map(|key| GraphRef { label: label.to_owned(), key }).collect();
    let params = GraphRetireParams { refs, reason: Some(reason.to_owned()), at_ms: None };
    host.call(methods::GRAPH_RETIRE, serde_json::to_value(params).map_err(|error| RpcError::internal(error.to_string()))?).await.map(|_| ())
}

/// The rows of one read-only statement.
pub async fn rows(host: &Host, cypher: &str, params: Value, limit: u32) -> Result<Vec<Vec<Value>>, RpcError> {
    let result: GraphQueryResult = host.get(methods::GRAPH_QUERY, json!({"cypher": cypher, "params": params, "limit": limit})).await?;
    Ok(result.rows)
}

/// The properties of the node each row returns first (`RETURN n`).
pub async fn nodes(host: &Host, cypher: &str, params: Value, limit: u32) -> Result<Vec<Map<String, Value>>, RpcError> {
    Ok(rows(host, cypher, params, limit)
        .await?
        .into_iter()
        .filter_map(|row| row.into_iter().next()?.get("properties").and_then(Value::as_object).cloned())
        .collect())
}

/// Append records to a stream, kept as long as the memory plugin keeps any.
pub async fn append(host: &Host, topic: &str, records: Vec<Value>) -> Result<(), RpcError> {
    if records.is_empty() {
        return Ok(());
    }
    let params = StreamAppendParams { topic: topic.to_owned(), records, retention_days: Some(3650) };
    host.effect(methods::STREAM_APPEND, serde_json::to_value(params).map_err(|error| RpcError::internal(error.to_string()))?).await.map(|_| ())
}

pub fn str_prop<'a>(props: &'a Map<String, Value>, name: &str) -> &'a str {
    props.get(name).and_then(Value::as_str).unwrap_or("")
}
