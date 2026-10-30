//! The plugin's records in the app's graph (`docs/GRAPH-MODEL.md`), served by the memory plugin
//! through `memory@1`: puts, read-only queries and stream appends.
//!
//! The memory plugin may start after this one. A call it does not answer yet is tried again every
//! second: a few times for a call the user waits on, until it is answered for the start-up work.
//! Writes that need no answer go through one queue, in order, and are tried again until memory
//! takes them; the plugin keeps working from memory meanwhile.

use cc_protocol::contracts::memory::{GraphLayer, GraphNode, GraphPutParams, GraphRef, StreamAppendParams, methods};
use cc_protocol::{Ctx, RpcError, codes};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

/// Between two attempts while memory does not answer.
const RETRY: Duration = Duration::from_secs(1);
/// Attempts for a call the user waits on.
const BOUNDED_TRIES: u32 = 30;

/// How long a call waits for memory to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// About half a minute: a call the user waits on.
    Bounded,
    /// Until it answers: start-up work and queued writes.
    Until,
}

/// A put built once the machine's key is known.
type Build = Box<dyn FnOnce(&str) -> GraphPutParams + Send>;

enum Write {
    Put(Build),
    Append(StreamAppendParams),
}

pub struct Graph {
    ctx: Arc<OnceLock<Ctx>>,
    machine: Mutex<Option<String>>,
    writes: mpsc::UnboundedSender<Write>,
}

/// The node `(label, key)`.
pub fn node_ref(label: &str, key: &str) -> GraphRef {
    GraphRef { label: label.to_owned(), key: key.to_owned() }
}

/// The machine's root node.
pub fn machine_ref(machine: &str) -> GraphRef {
    node_ref("Machine", machine)
}

/// A node with ordinary properties.
pub fn node(label: &str, key: &str, props: Map<String, Value>) -> GraphNode {
    GraphNode { label: label.to_owned(), key: key.to_owned(), props, temporal: Map::new() }
}

/// An error that means memory is not serving yet (or is being replaced).
fn not_serving(error: &RpcError) -> bool {
    matches!(error.code.as_str(), codes::UNAVAILABLE | codes::CONNECTION_CLOSED | codes::DRAINING)
}

impl Graph {
    /// The frame context arrives with the first call; the slot is filled then.
    pub fn new(ctx: Arc<OnceLock<Ctx>>) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let graph = Arc::new(Self { ctx, machine: Mutex::new(None), writes: tx });
        tokio::spawn(writer(graph.clone(), rx));
        graph
    }

    /// One call to memory, tried again while memory is not serving.
    pub async fn call(&self, method: &str, params: Value, wait: Wait) -> Result<Value, RpcError> {
        let mut tries = 0u32;
        loop {
            let outcome = match self.ctx.get() {
                Some(ctx) => ctx.call(method, params.clone()).await,
                None => Err(RpcError::new(codes::UNAVAILABLE, "not connected to the frame yet")),
            };
            match outcome {
                Err(error) if not_serving(&error) => {
                    tries += 1;
                    if tries == 1 {
                        tracing::info!(method, error = %error.message, "memory is not serving yet; trying again every second");
                    }
                    if wait == Wait::Bounded && tries >= BOUNDED_TRIES {
                        return Err(RpcError::new(codes::UNAVAILABLE, format!("memory did not answer {method}: {}; try again in a moment", error.message)).retryable());
                    }
                    tokio::time::sleep(RETRY).await;
                }
                other => return other,
            }
        }
    }

    /// The key of this machine's root node (asked once).
    pub async fn machine(&self, wait: Wait) -> Result<String, RpcError> {
        if let Some(key) = self.machine.lock().ok().and_then(|slot| slot.clone()) {
            return Ok(key);
        }
        let answer = self.call(methods::GRAPH_MACHINE, json!({}), wait).await?;
        let key = answer
            .get("key")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| RpcError::internal("memory named no machine"))?
            .to_owned();
        if let Ok(mut slot) = self.machine.lock() {
            *slot = Some(key.clone());
        }
        Ok(key)
    }

    /// Write a batch now and wait for memory to take it.
    pub async fn put(&self, build: impl FnOnce(&str) -> GraphPutParams, wait: Wait) -> Result<(), RpcError> {
        let machine = self.machine(wait).await?;
        let params = serde_json::to_value(build(&machine)).map_err(|error| RpcError::internal(error.to_string()))?;
        self.call(methods::GRAPH_PUT, params, wait).await.map(drop)
    }

    /// Append records to a stream now.
    pub async fn append(&self, topic: &str, records: Vec<Value>, wait: Wait) -> Result<(), RpcError> {
        let params = StreamAppendParams { topic: topic.to_owned(), records, retention_days: None };
        let params = serde_json::to_value(params).map_err(|error| RpcError::internal(error.to_string()))?;
        self.call(methods::STREAM_APPEND, params, wait).await.map(drop)
    }

    /// Write a batch in the background, after the writes queued before it.
    pub fn put_later(&self, build: impl FnOnce(&str) -> GraphPutParams + Send + 'static) {
        let _ = self.writes.send(Write::Put(Box::new(build)));
    }

    /// Append records to a stream in the background, after the writes queued before them.
    pub fn append_later(&self, topic: &str, records: Vec<Value>) {
        let _ = self.writes.send(Write::Append(StreamAppendParams { topic: topic.to_owned(), records, retention_days: None }));
    }

    /// One read-only statement; the rows' columns as JSON values.
    pub async fn query(&self, cypher: &str, params: Value, wait: Wait) -> Result<Vec<Vec<Value>>, RpcError> {
        let answer = self.call(methods::GRAPH_QUERY, json!({ "cypher": cypher, "params": params, "limit": 5000 }), wait).await?;
        let rows = answer.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(rows.into_iter().map(|row| row.as_array().cloned().unwrap_or_default()).collect())
    }

    /// The properties of the node each row returns first (`RETURN n`).
    pub async fn nodes(&self, cypher: &str, params: Value, wait: Wait) -> Result<Vec<Map<String, Value>>, RpcError> {
        let rows = self.query(cypher, params, wait).await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.into_iter().next()?.get("properties")?.as_object().cloned())
            .collect())
    }
}

/// A batch in the app's own layer (`workspace`).
pub fn workspace_batch(nodes: Vec<GraphNode>, edges: Vec<cc_protocol::contracts::memory::GraphEdge>) -> GraphPutParams {
    GraphPutParams { nodes, edges, layer: GraphLayer::Workspace, at_ms: None }
}

/// An edge without properties.
pub fn edge(from: GraphRef, rel: &str, to: GraphRef) -> cc_protocol::contracts::memory::GraphEdge {
    cc_protocol::contracts::memory::GraphEdge { from, rel: rel.to_owned(), to, props: Map::new(), temporal: Map::new() }
}

/// Queued writes, in order; each is tried until memory takes it. A batch memory refuses is
/// logged and dropped: sending it again would be refused again.
async fn writer(graph: Arc<Graph>, mut rx: mpsc::UnboundedReceiver<Write>) {
    while let Some(write) = rx.recv().await {
        let outcome = match write {
            Write::Put(build) => match graph.machine(Wait::Until).await {
                Ok(machine) => match serde_json::to_value(build(&machine)) {
                    Ok(params) => graph.call(methods::GRAPH_PUT, params, Wait::Until).await.map(drop),
                    Err(error) => Err(RpcError::internal(error.to_string())),
                },
                Err(error) => Err(error),
            },
            Write::Append(params) => match serde_json::to_value(params) {
                Ok(params) => graph.call(methods::STREAM_APPEND, params, Wait::Until).await.map(drop),
                Err(error) => Err(RpcError::internal(error.to_string())),
            },
        };
        if let Err(error) = outcome {
            tracing::error!(error = %error.message, "memory refused a record of this plugin; it was not kept");
        }
    }
}
