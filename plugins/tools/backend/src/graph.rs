//! The plugin's records in the app's graph (`memory@1`, `docs/GRAPH-MODEL.md`): steps, jobs,
//! workspace clones, artifacts, venvs and approvals are nodes and edges; step and job events are
//! streams. Nothing of them is kept in a file.
//!
//! Writes go through one queue, in order, so a node is written before the edges that need it. A
//! write the memory plugin cannot take yet (it starts after this plugin, or is being swapped) is
//! tried again every second until it is taken; the plugin keeps working from its memory meanwhile.
//! A batch the memory plugin refuses is written once more without its soft edges (to a session or
//! a project it may not know), and dropped with an error in the log when that is refused too.
//!
//! `Machine` is the root: a ref made by [`machine`] is filled with this Mac's key when written.

use crate::host::Host;
use cc_protocol::contracts::memory::{
    GraphEdge, GraphLayer, GraphNode, GraphPutParams, GraphQueryParams, GraphQueryResult, GraphRef, GraphRetireParams, MachineInfo, StreamAppendParams,
    methods,
};
use cc_protocol::{RpcError, codes};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Nodes and edges together per put, and refs per retire.
const MAX_ITEMS: usize = 5000;
/// Rows a paged query asks for at once.
pub const PAGE: u32 = 5000;
/// Between two attempts while the memory plugin does not serve.
const RETRY: Duration = Duration::from_secs(1);

/// One unit of a put: nodes, the edges that must be written with them, and soft edges that may be
/// left out when the batch is refused.
#[derive(Debug, Clone)]
pub struct Batch {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub soft: Vec<GraphEdge>,
    /// Where new nodes go: `workspace` for the plugin's own records, `observed` for steps.
    pub layer: GraphLayer,
}

impl Batch {
    pub fn workspace(nodes: Vec<GraphNode>, edges: Vec<GraphEdge>, soft: Vec<GraphEdge>) -> Batch {
        Batch { nodes, edges, soft, layer: GraphLayer::Workspace }
    }

    pub fn observed(nodes: Vec<GraphNode>, edges: Vec<GraphEdge>, soft: Vec<GraphEdge>) -> Batch {
        Batch { nodes, edges, soft, layer: GraphLayer::Observed }
    }

    fn items(&self) -> usize {
        self.nodes.len() + self.edges.len() + self.soft.len()
    }
}

enum Op {
    Put { batches: Vec<Batch>, done: Option<oneshot::Sender<Result<(), RpcError>>> },
    Retire { refs: Vec<GraphRef>, reason: String },
    /// Retire every node whose key the query's first column returns (it filters retired ones out,
    /// so it is asked again until it returns none).
    RetireMatching { label: String, cypher: String, params: Map<String, Value>, reason: String },
    Append { topic: String, records: Vec<Value> },
    Flush(oneshot::Sender<()>),
}

pub struct Graph {
    tx: mpsc::UnboundedSender<Op>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Op>>>,
    host: OnceLock<Arc<dyn Host>>,
}

/// A node ref.
pub fn gref(label: &str, key: &str) -> GraphRef {
    GraphRef { label: label.to_owned(), key: key.to_owned() }
}

/// This Mac's root node (its key is filled in when written).
pub fn machine() -> GraphRef {
    gref("Machine", "")
}

pub fn node(label: &str, key: &str, props: Map<String, Value>) -> GraphNode {
    GraphNode { label: label.to_owned(), key: key.to_owned(), props, temporal: Map::new() }
}

pub fn edge(from: &GraphRef, rel: &str, to: &GraphRef) -> GraphEdge {
    GraphEdge { from: from.clone(), rel: rel.to_owned(), to: to.clone(), props: Map::new(), temporal: Map::new() }
}

/// Properties from a JSON object (`json!({...})`); `null` values are left out.
pub fn props(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map.into_iter().filter(|(_, value)| !value.is_null()).collect(),
        _ => Map::new(),
    }
}

/// The properties of a node a query returned (`{id, labels, properties}`).
pub fn properties(value: &Value) -> Option<&Map<String, Value>> {
    value.get("properties").and_then(Value::as_object)
}

pub fn text(props: &Map<String, Value>, name: &str) -> String {
    props.get(name).and_then(Value::as_str).unwrap_or_default().to_owned()
}

pub fn number(props: &Map<String, Value>, name: &str) -> u64 {
    props.get(name).and_then(|value| value.as_u64().or_else(|| value.as_f64().map(|f| f as u64))).unwrap_or(0)
}

fn transient(error: &RpcError) -> bool {
    matches!(error.code.as_str(), codes::UNAVAILABLE | codes::CONNECTION_CLOSED | codes::DRAINING)
}

impl Graph {
    pub fn new() -> Arc<Graph> {
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(Graph { tx, rx: Mutex::new(Some(rx)), host: OnceLock::new() })
    }

    /// Start writing through `host` (the frame connection). Writes queued before wait for this.
    pub fn attach(&self, host: Arc<dyn Host>) {
        let Some(rx) = self.rx.lock().ok().and_then(|mut rx| rx.take()) else { return };
        let _ = self.host.set(host.clone());
        tokio::spawn(write(host, rx));
    }

    pub fn put(&self, batch: Batch) {
        self.put_many(vec![batch]);
    }

    pub fn put_many(&self, batches: Vec<Batch>) {
        let _ = self.tx.send(Op::Put { batches, done: None });
    }

    /// Put and wait until the graph took it (or refused it).
    pub async fn put_wait(&self, batches: Vec<Batch>) -> Result<(), RpcError> {
        let (done, taken) = oneshot::channel();
        self.tx.send(Op::Put { batches, done: Some(done) }).map_err(|_| RpcError::internal("the graph writer stopped"))?;
        taken.await.map_err(|_| RpcError::internal("the graph writer stopped before the write was taken"))?
    }

    pub fn retire(&self, refs: Vec<GraphRef>, reason: &str) {
        if !refs.is_empty() {
            let _ = self.tx.send(Op::Retire { refs, reason: reason.to_owned() });
        }
    }

    /// Retire every `label` node whose key `cypher` returns in its first column. The query must
    /// leave retired nodes out (`n.ended_ms IS NULL`) and end with `LIMIT`.
    pub fn retire_matching(&self, label: &str, cypher: &str, params: Value, reason: &str) {
        let params = match params {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        let _ = self.tx.send(Op::RetireMatching { label: label.to_owned(), cypher: cypher.to_owned(), params, reason: reason.to_owned() });
    }

    pub fn append(&self, topic: &str, records: Vec<Value>) {
        if !records.is_empty() {
            let _ = self.tx.send(Op::Append { topic: topic.to_owned(), records });
        }
    }

    /// Wait until what was queued before is written, at most `limit` (before a successor takes over).
    pub async fn flush(&self, limit: Duration) {
        let (done, flushed) = oneshot::channel();
        if self.tx.send(Op::Flush(done)).is_ok() && tokio::time::timeout(limit, flushed).await.is_err() {
            tracing::warn!("the graph did not take the last writes before the hand-over");
        }
    }

    /// One read-only query; `attempts` bounds the wait while the memory plugin does not serve yet.
    pub async fn query(&self, cypher: &str, params: Value, limit: u32, attempts: u32) -> Result<GraphQueryResult, RpcError> {
        let host = self.host.get().cloned().ok_or_else(|| RpcError::internal("the plugin has no frame connection"))?;
        let params = match params {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        let request = serde_json::to_value(GraphQueryParams { cypher: cypher.to_owned(), params, limit: Some(limit) }).map_err(|error| RpcError::internal(error.to_string()))?;
        let mut tries = 0;
        loop {
            match host.call(methods::GRAPH_QUERY, request.clone()).await {
                Ok(value) => return serde_json::from_value(value).map_err(|error| RpcError::internal(format!("a graph answer cannot be read: {error}"))),
                Err(error) if transient(&error) && tries + 1 < attempts => {
                    tries += 1;
                    if tries == 1 {
                        tracing::info!(error = %error.message, "the memory plugin does not serve yet; asking again");
                    }
                    tokio::time::sleep(RETRY).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Call `method` of the memory plugin, waiting while it does not serve yet.
    pub async fn call_waiting(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let host = self.host.get().cloned().ok_or_else(|| RpcError::internal("the plugin has no frame connection"))?;
        call(&host, method, params).await
    }

    /// Every row of a query paged by key: `cypher` filters on `$after` (`n.key > $after`), orders
    /// by that key, ends with `LIMIT 5000`, and returns the key as its last column.
    pub async fn query_all(&self, cypher: &str, params: Value, attempts: u32) -> Result<Vec<Vec<Value>>, RpcError> {
        let mut rows = Vec::new();
        let mut after = String::new();
        loop {
            let mut page_params = params.clone();
            if let Some(map) = page_params.as_object_mut() {
                map.insert("after".to_owned(), json!(after));
            }
            let page = self.query(cypher, page_params, PAGE, attempts).await?;
            let count = page.rows.len();
            let last = page.rows.last().and_then(|row| row.last()).and_then(Value::as_str).map(str::to_owned);
            rows.extend(page.rows);
            match last {
                Some(key) if count as u32 >= PAGE && key > after => after = key,
                _ => return Ok(rows),
            }
        }
    }
}

/// Call `method` until the memory plugin serves it.
async fn call(host: &Arc<dyn Host>, method: &str, params: Value) -> Result<Value, RpcError> {
    let mut waited = false;
    loop {
        match host.call(method, params.clone()).await {
            Err(error) if transient(&error) => {
                if !waited {
                    tracing::info!(method, error = %error.message, "the memory plugin does not serve yet; writing when it does");
                    waited = true;
                }
                tokio::time::sleep(RETRY).await;
            }
            outcome => return outcome,
        }
    }
}

async fn write(host: Arc<dyn Host>, mut rx: mpsc::UnboundedReceiver<Op>) {
    let mut machine: Option<String> = None;
    while let Some(op) = rx.recv().await {
        match op {
            Op::Flush(done) => {
                let _ = done.send(());
            }
            Op::Append { topic, records } => {
                for chunk in records.chunks(MAX_ITEMS) {
                    let params = json!(StreamAppendParams { topic: topic.clone(), records: chunk.to_vec(), retention_days: None });
                    if let Err(error) = call(&host, methods::STREAM_APPEND, params).await {
                        tracing::error!(topic, error = %error.message, "the step or job event was not kept");
                    }
                }
            }
            Op::Retire { refs, reason } => retire(&host, refs, &reason).await,
            Op::RetireMatching { label, cypher, params, reason } => {
                // Bounded: each round retires what the last returned, so the query shrinks.
                for _ in 0..1000 {
                    let request = json!(GraphQueryParams { cypher: cypher.clone(), params: params.clone(), limit: Some(PAGE) });
                    let rows = match call(&host, methods::GRAPH_QUERY, request).await.and_then(|value| {
                        serde_json::from_value::<GraphQueryResult>(value).map_err(|error| RpcError::internal(error.to_string()))
                    }) {
                        Ok(result) => result.rows,
                        Err(error) => {
                            tracing::error!(label, error = %error.message, "the records to retire could not be found");
                            break;
                        }
                    };
                    let refs: Vec<GraphRef> = rows.iter().filter_map(|row| row.first()?.as_str().map(|key| gref(&label, key))).collect();
                    if refs.is_empty() {
                        break;
                    }
                    retire(&host, refs, &reason).await;
                }
            }
            Op::Put { batches, done } => {
                if machine.is_none() {
                    machine = match call(&host, methods::GRAPH_MACHINE, json!({})).await.and_then(|value| {
                        serde_json::from_value::<MachineInfo>(value).map_err(|error| RpcError::internal(error.to_string()))
                    }) {
                        Ok(info) => Some(info.key),
                        Err(error) => {
                            tracing::error!(error = %error.message, "this Mac's graph node is unknown; the records are not written");
                            if let Some(done) = done {
                                let _ = done.send(Err(error));
                            }
                            continue;
                        }
                    };
                }
                let key = machine.clone().unwrap_or_default();
                let outcome = put(&host, &key, batches).await;
                if let Some(done) = done {
                    let _ = done.send(outcome);
                }
            }
        }
    }
}

async fn retire(host: &Arc<dyn Host>, refs: Vec<GraphRef>, reason: &str) {
    for chunk in refs.chunks(MAX_ITEMS) {
        let params = json!(GraphRetireParams { refs: chunk.to_vec(), reason: Some(reason.to_owned()), at_ms: None });
        if let Err(error) = call(host, methods::GRAPH_RETIRE, params).await {
            tracing::error!(error = %error.message, "records were not retired");
        }
    }
}

fn fill(reference: &mut GraphRef, machine: &str) {
    if reference.label == "Machine" && reference.key.is_empty() {
        reference.key = machine.to_owned();
    }
}

/// Batches packed into puts of at most [`MAX_ITEMS`], in order.
async fn put(host: &Arc<dyn Host>, machine: &str, batches: Vec<Batch>) -> Result<(), RpcError> {
    let mut groups: Vec<Batch> = Vec::new();
    for mut batch in batches {
        for edge in batch.edges.iter_mut().chain(batch.soft.iter_mut()) {
            fill(&mut edge.from, machine);
            fill(&mut edge.to, machine);
        }
        match groups.last_mut() {
            Some(last) if last.layer == batch.layer && last.items() + batch.items() <= MAX_ITEMS => {
                last.nodes.extend(batch.nodes);
                last.edges.extend(batch.edges);
                last.soft.extend(batch.soft);
            }
            _ => groups.push(batch),
        }
    }
    let mut outcome = Ok(());
    for group in groups {
        let with_soft = GraphPutParams { nodes: group.nodes.clone(), edges: group.edges.iter().chain(&group.soft).cloned().collect(), layer: group.layer, at_ms: None };
        let refused = match call(host, methods::GRAPH_PUT, json!(with_soft)).await {
            Ok(_) => continue,
            Err(error) => error,
        };
        if group.soft.is_empty() {
            tracing::error!(error = %refused.message, "the graph refused records; they are not kept");
            outcome = Err(refused);
            continue;
        }
        let without = GraphPutParams { nodes: group.nodes, edges: group.edges, layer: group.layer, at_ms: None };
        if let Err(error) = call(host, methods::GRAPH_PUT, json!(without)).await {
            tracing::error!(error = %error.message, first = %refused.message, "the graph refused records; they are not kept");
            outcome = Err(error);
        } else {
            tracing::warn!(error = %refused.message, "records were kept without their links to a session or project");
        }
    }
    outcome
}
