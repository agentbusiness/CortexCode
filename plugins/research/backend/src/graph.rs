//! Research's records in the memory plugin's graph (memory@1: `memory.graph.*`, `memory.stream.*`),
//! called through the frame. The deep jobs (`jobstore`) and the fetch cache (`fetch`) keep their
//! records here; nothing else holds them.
//!
//! Memory may start after this plugin, or restart: a call it refuses as away (`unavailable`,
//! `connection_closed`, `draining`) is tried again every second, up to the caller's patience.

use crate::peers::Peers;
use cc_protocol::contracts::memory::{GraphEdge, GraphLayer, GraphNode, GraphPutParams, GraphPutResult, GraphQueryParams, GraphQueryResult, GraphRef, GraphRetireParams, GraphRetireResult, MachineInfo, StreamAppendParams, StreamAppendResult, methods};
use cc_protocol::{RpcError, codes, parse_params, to_value};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

/// Tries while memory is away, one second apart: a minute for writes and loads.
pub const PATIENT: u32 = 60;
/// Tries for a lookup on a request's path (a cache read): a miss beats a long wait.
pub const BRIEF: u32 = 3;
/// The longest key the graph takes (bytes).
const KEY_BYTES: usize = 1024;

/// Whether memory refused a call as away (worth trying again).
pub fn away(error: &RpcError) -> bool {
    matches!(error.code.as_str(), codes::UNAVAILABLE | codes::CONNECTION_CLOSED | codes::DRAINING)
}

pub struct Graph {
    peers: Arc<dyn Peers>,
    machine: OnceCell<String>,
}

impl Graph {
    pub fn new(peers: Arc<dyn Peers>) -> Self {
        Self { peers, machine: OnceCell::new() }
    }

    async fn call<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: &P, tries: u32) -> Result<R, RpcError> {
        let value = to_value(params)?;
        let mut attempt = 1;
        loop {
            match self.peers.call(method, value.clone(), CancellationToken::new()).await {
                Ok(answer) => return parse_params(answer),
                Err(error) if away(&error) && attempt < tries => {
                    if attempt == 1 {
                        tracing::info!("research: memory is away ({method}: {}); trying again every second", error.message);
                    }
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(error) => {
                    let code = if error.code == codes::UNKNOWN_METHOD { codes::UNAVAILABLE } else { error.code.as_str() };
                    return Err(RpcError::new(code, format!("research keeps its records in the memory plugin's graph (memory@1): {method}: {}", error.message)));
                }
            }
        }
    }

    /// This Mac's root node key (asked once).
    pub async fn machine(&self) -> Result<String, RpcError> {
        self.machine
            .get_or_try_init(|| async {
                let info: MachineInfo = self.call(methods::GRAPH_MACHINE, &json!({}), PATIENT).await?;
                Ok::<_, RpcError>(info.key)
            })
            .await
            .cloned()
    }

    /// Merge nodes and edges into the workspace layer.
    pub async fn put(&self, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>, tries: u32) -> Result<(), RpcError> {
        if nodes.is_empty() && edges.is_empty() {
            return Ok(());
        }
        let params = GraphPutParams { nodes, edges, layer: GraphLayer::Workspace, at_ms: None };
        let _: GraphPutResult = self.call(methods::GRAPH_PUT, &params, tries).await?;
        Ok(())
    }

    /// Mark nodes as gone (a later put brings them back).
    pub async fn retire(&self, refs: Vec<GraphRef>, reason: &str, tries: u32) -> Result<(), RpcError> {
        if refs.is_empty() {
            return Ok(());
        }
        let params = GraphRetireParams { refs, reason: Some(reason.to_owned()), at_ms: None };
        let _: GraphRetireResult = self.call(methods::GRAPH_RETIRE, &params, tries).await?;
        Ok(())
    }

    /// One read-only statement; its rows.
    pub async fn query(&self, cypher: &str, params: Value, limit: u32, tries: u32) -> Result<Vec<Vec<Value>>, RpcError> {
        let params = GraphQueryParams { cypher: cypher.to_owned(), params: params.as_object().cloned().unwrap_or_default(), limit: Some(limit) };
        let result: GraphQueryResult = self.call(methods::GRAPH_QUERY, &params, tries).await?;
        Ok(result.rows)
    }

    pub async fn append(&self, topic: &str, records: Vec<Value>, tries: u32) -> Result<(), RpcError> {
        let params = StreamAppendParams { topic: topic.to_owned(), records, retention_days: Some(365) };
        let _: StreamAppendResult = self.call(methods::STREAM_APPEND, &params, tries).await?;
        Ok(())
    }
}

pub fn gref(label: &str, key: &str) -> GraphRef {
    GraphRef { label: label.to_owned(), key: key.to_owned() }
}

pub fn node(label: &str, key: &str, props: Map<String, Value>) -> GraphNode {
    GraphNode { label: label.to_owned(), key: key.to_owned(), props, temporal: Map::new() }
}

pub fn edge(from: GraphRef, rel: &str, to: GraphRef) -> GraphEdge {
    GraphEdge { from, rel: rel.to_owned(), to, props: Map::new(), temporal: Map::new() }
}

/// A key from free text (a URL): the text when it fits, else its SHA-256.
pub fn fit_key(text: &str) -> String {
    if text.len() <= KEY_BYTES { text.to_owned() } else { format!("sha256:{}", crate::text::sha256_hex(text.as_bytes())) }
}

/// A node cell of a query row: its properties.
pub fn props_of(cell: &Value) -> Map<String, Value> {
    cell.get("properties").and_then(Value::as_object).cloned().unwrap_or_default()
}

pub fn labels_of(cell: &Value) -> Vec<String> {
    cell.get("labels").and_then(Value::as_array).map(|l| l.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

pub fn text(props: &Map<String, Value>, name: &str) -> String {
    props.get(name).and_then(Value::as_str).unwrap_or_default().to_owned()
}

pub fn opt_text(props: &Map<String, Value>, name: &str) -> Option<String> {
    props.get(name).and_then(Value::as_str).map(str::to_owned)
}

pub fn number(props: &Map<String, Value>, name: &str) -> u64 {
    props.get(name).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))).unwrap_or(0)
}

pub fn opt_number(props: &Map<String, Value>, name: &str) -> Option<u64> {
    props.get(name).filter(|v| !v.is_null()).map(|_| number(props, name))
}

pub fn flag(props: &Map<String, Value>, name: &str) -> bool {
    props.get(name).and_then(Value::as_bool).unwrap_or(false)
}

pub fn texts(props: &Map<String, Value>, name: &str) -> Vec<String> {
    props.get(name).and_then(Value::as_array).map(|l| l.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// Optional values as properties: `None` removes the property.
pub fn or_null<T: Serialize>(value: &Option<T>) -> Value {
    value.as_ref().map(|v| serde_json::to_value(v).unwrap_or(Value::Null)).unwrap_or(Value::Null)
}
