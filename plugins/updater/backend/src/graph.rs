//! The plugin's records in the app's graph (`memory@1`, `docs/GRAPH-MODEL.md`), written in order.
//!
//! Every put, retire and stream append goes through one writer task, in the order it was asked
//! for, in the WORKSPACE layer. The memory plugin may start after this one: the writer waits for
//! it (a call it cannot serve yet is tried again each second) and the plugin keeps working from
//! its own memory meanwhile. A batch the memory plugin refuses is logged and dropped; [`Graph::flush`]
//! reports it, so a migration keeps its old file.
//!
//! An edge to [`machine`] names this Mac's root node; the writer fills in its key. A put may carry
//! other edges to use when the memory plugin refuses its own ([`Graph::put_or`]): a session it does
//! not know, say.

use cc_protocol::contracts::memory::{
    GraphEdge, GraphLayer, GraphNode, GraphPutParams, GraphQueryParams, GraphQueryResult, GraphRef, GraphRetireParams, MachineInfo, StreamAppendParams,
    StreamEntry, StreamReadParams, StreamReadResult, StreamTopicsParams, StreamTopicsResult, methods,
};
use cc_protocol::{BoxFuture, Ctx, RpcError, codes};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Between two attempts while the memory plugin does not serve.
const RETRY: Duration = Duration::from_secs(1);
/// How long a read waits for the memory plugin before it gives up.
const READ_WAIT: Duration = Duration::from_secs(120);
/// The label of the root node.
const MACHINE: &str = "Machine";

/// How the plugin calls the frame.
pub type Caller = Arc<dyn Fn(&str, Value) -> BoxFuture<'static, Result<Value, RpcError>> + Send + Sync>;

/// The frame connection as a [`Caller`].
pub fn ctx_caller(ctx: &Ctx) -> Caller {
    let ctx = ctx.clone();
    Arc::new(move |method: &str, params: Value| {
        let (ctx, method) = (ctx.clone(), method.to_owned());
        Box::pin(async move { ctx.call(&method, params).await })
    })
}

/// This Mac's root node (the writer fills in its key).
pub fn machine() -> GraphRef {
    GraphRef { label: MACHINE.into(), key: String::new() }
}

/// A node by its label and key.
pub fn at(label: &str, key: &str) -> GraphRef {
    GraphRef { label: label.into(), key: key.into() }
}

pub fn node(label: &str, key: &str, props: Map<String, Value>) -> GraphNode {
    GraphNode { label: label.into(), key: key.into(), props, temporal: Map::new() }
}

pub fn edge(from: (&str, &str), rel: &str, to: GraphRef) -> GraphEdge {
    GraphEdge { from: GraphRef { label: from.0.into(), key: from.1.into() }, rel: rel.into(), to, props: Map::new(), temporal: Map::new() }
}

/// An answer that means the memory plugin does not serve (yet).
fn waiting(error: &RpcError) -> bool {
    matches!(error.code.as_str(), codes::UNAVAILABLE | codes::CONNECTION_CLOSED | codes::DRAINING | codes::UNKNOWN_METHOD)
}

enum Op {
    /// A put, and the edges to try instead when it is refused.
    Put(GraphPutParams, Option<Vec<GraphEdge>>),
    Retire(GraphRetireParams),
    Append(StreamAppendParams),
    /// Answers whether every write since the last flush was taken.
    Flush(oneshot::Sender<bool>),
}

pub struct Graph {
    ops: mpsc::UnboundedSender<Op>,
    queued: Mutex<Option<mpsc::UnboundedReceiver<Op>>>,
    caller: Mutex<Option<Caller>>,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    /// Writes asked for before [`Graph::start`] wait in order.
    pub fn new() -> Self {
        let (ops, queued) = mpsc::unbounded_channel();
        Self { ops, queued: Mutex::new(Some(queued)), caller: Mutex::new(None) }
    }

    /// Start writing through `caller` (once; a second call does nothing). Runs inside Tokio.
    pub fn start(&self, caller: Caller) {
        let Some(queued) = self.queued.lock().unwrap_or_else(|p| p.into_inner()).take() else { return };
        *self.caller.lock().unwrap_or_else(|p| p.into_inner()) = Some(caller.clone());
        tokio::spawn(write_all(caller, queued));
    }

    fn caller(&self) -> Result<Caller, RpcError> {
        self.caller.lock().unwrap_or_else(|p| p.into_inner()).clone().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "not connected to the frame yet"))
    }

    /// Merge nodes and edges (WORKSPACE layer).
    pub fn put(&self, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) {
        if nodes.is_empty() && edges.is_empty() {
            return;
        }
        let _ = self.ops.send(Op::Put(GraphPutParams { nodes, edges, layer: GraphLayer::Workspace, at_ms: None }, None));
    }

    /// As [`Graph::put`]; when the memory plugin refuses it, the nodes go with `fallback` edges.
    pub fn put_or(&self, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>, fallback: Vec<GraphEdge>) {
        let _ = self.ops.send(Op::Put(GraphPutParams { nodes, edges, layer: GraphLayer::Workspace, at_ms: None }, Some(fallback)));
    }

    /// Mark nodes as gone (`ended_ms`); a later put brings one back.
    pub fn retire(&self, refs: Vec<GraphRef>, reason: &str) {
        if refs.is_empty() {
            return;
        }
        let _ = self.ops.send(Op::Retire(GraphRetireParams { refs, reason: Some(reason.into()), at_ms: None }));
    }

    /// Append records to a stream.
    pub fn append(&self, topic: &str, records: Vec<Value>, retention_days: Option<u32>) {
        if records.is_empty() {
            return;
        }
        let _ = self.ops.send(Op::Append(StreamAppendParams { topic: topic.into(), records, retention_days }));
    }

    /// Wait until the writes asked for so far are done: true when the memory plugin took them all.
    pub async fn flush(&self) -> bool {
        let (done, answer) = oneshot::channel();
        if self.ops.send(Op::Flush(done)).is_err() {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    /// Any other call to the memory plugin (a settings read for a migration), waiting a bounded
    /// time for it to serve.
    pub async fn ask<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, RpcError> {
        read(&self.caller()?, method, params).await
    }

    /// One read-only statement; each row's first column, a node, as its properties.
    pub async fn nodes(&self, cypher: &str, params: Map<String, Value>, limit: u32) -> Result<Vec<Map<String, Value>>, RpcError> {
        let query = GraphQueryParams { cypher: cypher.into(), params, limit: Some(limit) };
        let result: GraphQueryResult = read(&self.caller()?, methods::GRAPH_QUERY, json!(query)).await?;
        Ok(result
            .rows
            .into_iter()
            .filter_map(|row| row.into_iter().next())
            .filter_map(|node| match node.get("properties") {
                Some(Value::Object(props)) => Some(props.clone()),
                _ => None,
            })
            .collect())
    }

    /// The newest `count` records of a stream, oldest first (none when it has none yet).
    pub async fn newest(&self, topic: &str, count: u64) -> Result<Vec<StreamEntry>, RpcError> {
        let caller = self.caller()?;
        let topics: StreamTopicsResult = read(&caller, methods::STREAM_TOPICS, json!(StreamTopicsParams { prefix: topic.into() })).await?;
        let Some(end) = topics.topics.iter().find(|t| t.name == topic).map(|t| t.next_offset) else { return Ok(Vec::new()) };
        let params = StreamReadParams { topic: topic.into(), from_offset: Some(end.saturating_sub(count)), since_ms: None, limit: Some(count.clamp(1, 5000) as u32) };
        let result: StreamReadResult = read(&caller, methods::STREAM_READ, json!(params)).await?;
        Ok(result.records)
    }
}

/// A read, waiting a bounded time for the memory plugin.
async fn read<T: DeserializeOwned>(caller: &Caller, method: &str, params: Value) -> Result<T, RpcError> {
    let started = std::time::Instant::now();
    loop {
        match caller(method, params.clone()).await {
            Ok(value) => return serde_json::from_value(value).map_err(|e| RpcError::internal(format!("{method}: unexpected answer: {e}"))),
            Err(error) if waiting(&error) && started.elapsed() < READ_WAIT => {
                if started.elapsed() < RETRY {
                    tracing::info!(method, error = %error.message, "the memory plugin does not serve yet; waiting");
                }
                tokio::time::sleep(RETRY).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// A write, tried again for as long as the memory plugin does not serve.
async fn write(caller: &Caller, method: &str, params: Value) -> Result<Value, RpcError> {
    let mut attempts: u64 = 0;
    loop {
        match caller(method, params.clone()).await {
            Err(error) if waiting(&error) => {
                if attempts % 60 == 0 {
                    tracing::info!(method, error = %error.message, "the memory plugin does not serve yet; the write waits");
                }
                attempts += 1;
                tokio::time::sleep(RETRY).await;
            }
            outcome => return outcome,
        }
    }
}

async fn write_all(caller: Caller, mut ops: mpsc::UnboundedReceiver<Op>) {
    let mut machine_key: Option<String> = None;
    let mut all_taken = true;
    while let Some(op) = ops.recv().await {
        let outcome = match op {
            Op::Flush(done) => {
                let _ = done.send(std::mem::replace(&mut all_taken, true));
                continue;
            }
            Op::Retire(params) => write(&caller, methods::GRAPH_RETIRE, json!(params)).await,
            Op::Append(params) => write(&caller, methods::STREAM_APPEND, json!(params)).await,
            Op::Put(mut params, fallback) => match put(&caller, &mut machine_key, &mut params).await {
                Err(error) if fallback.is_some() => {
                    tracing::info!(error = %error.message, "the memory plugin refused a put's edges; it goes with its other edges");
                    params.edges = fallback.unwrap_or_default();
                    put(&caller, &mut machine_key, &mut params).await
                }
                outcome => outcome,
            },
        };
        if let Err(error) = outcome {
            tracing::warn!(error = %error.message, "the memory plugin refused a write; it is dropped");
            all_taken = false;
        }
    }
}

/// A put, its edges to [`machine`] given this Mac's key (asked once).
async fn put(caller: &Caller, machine_key: &mut Option<String>, params: &mut GraphPutParams) -> Result<Value, RpcError> {
    if params.edges.iter().any(|edge| is_machine(&edge.from) || is_machine(&edge.to)) {
        if machine_key.is_none() {
            let info = write(caller, methods::GRAPH_MACHINE, json!({})).await?;
            let info: MachineInfo = serde_json::from_value(info).map_err(|e| RpcError::internal(format!("this Mac's root node: {e}")))?;
            *machine_key = Some(info.key);
        }
        let key = machine_key.clone().unwrap_or_default();
        for edge in &mut params.edges {
            for end in [&mut edge.from, &mut edge.to] {
                if is_machine(end) {
                    end.key = key.clone();
                }
            }
        }
    }
    write(caller, methods::GRAPH_PUT, json!(params)).await
}

fn is_machine(end: &GraphRef) -> bool {
    end.label == MACHINE && end.key.is_empty()
}

/// Properties from `(name, value)` pairs; `Value::Null` removes a property.
pub fn props<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
    pairs.into_iter().map(|(name, value)| (name.to_owned(), value)).collect()
}

pub fn text(props: &Map<String, Value>, name: &str) -> String {
    props.get(name).and_then(Value::as_str).unwrap_or_default().to_owned()
}

pub fn number(props: &Map<String, Value>, name: &str) -> Option<u64> {
    props.get(name).and_then(Value::as_u64)
}

pub fn flag(props: &Map<String, Value>, name: &str) -> bool {
    props.get(name).and_then(Value::as_bool).unwrap_or(false)
}

pub fn texts(props: &Map<String, Value>, name: &str) -> Vec<String> {
    props.get(name).and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}
