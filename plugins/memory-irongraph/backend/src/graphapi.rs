//! The terminal's graph (`docs/GRAPH-MODEL.md`): every CortexTerm plugin's data as nodes and edges
//! of its own labels in the main project (`db`), written through `memory.graph.put` and read back
//! with read-only Cypher (`memory.graph.query`).
//!
//! ```text
//! (:Machine {key: hardware uuid, name, os, first_ms, last_ms})      the root, written at start
//! (:<Label> {key, first_ms, last_ms, ended_ms?, ended_reason?, state?, …props})   unique by key
//!     -[:<REL> {first_ms, last_ms, …props}]->(:<Label>)                unique by (from, rel, to)
//! ```
//!
//! **How a put is written.** The batch is checked whole before anything is written: names (labels
//! CamelCase, relationship types UPPER_SNAKE, properties snake_case; nothing but these validated
//! names is ever put into a statement, every value goes in as a parameter), sizes, value types,
//! that every node has an edge (in the batch, or one already stored; `Machine` stands alone), and
//! that every edge's ends exist (in the batch, or stored). One read per label answers the last two.
//! An edge's end may also be a `Project`, `Session` or `Knowledge` item, records the memory plugin
//! keeps itself, named by its `id`:
//! it must be stored, and the put only links to it (never creates or changes it).
//! Then each label gets its key constraint (once per process), each temporal property its
//! declaration (once per process), and the batch is written as `UNWIND` statements: one per
//! label for the nodes, one per (from label, type, to label) for the edges, and one per temporal
//! property of each (in chunks of [`CHUNK`] rows). Each statement is atomic; IronGraph has no multi-statement transactions, so an
//! engine failure part-way leaves the statements before it written, and repeating the call is
//! safe: everything merges. Writes are serialised, so the checks hold for the writes that follow.
//!
//! **Temporal properties.** IronGraph declares a property temporal per label (or relationship
//! type) with `ALTER NODE PROPERTY L.p SET TEMPORAL <type> RETENTION …`, which only takes effect
//! on a property some write already made, and turns its ordinary values into nothing. The
//! declaration therefore writes the property once on a sentinel node of the label, declares it,
//! and deletes the sentinel; repeating it is harmless (same type: no change, history kept), so a
//! new process declares again on first use. Samples are written `AT TIME datetime.fromepochmillis
//! ($at)`, only when the value differs from the current one (a history of changes; a repeated
//! put adds no sample). The current value is the latest sample.

use crate::db::{Graph, layer};
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{GraphEdge, GraphLayer, GraphNode, GraphPutParams, GraphPutResult, GraphQueryParams, GraphQueryResult, GraphRef, GraphRetireParams, MachineInfo};
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Nodes and edges together per put.
pub const MAX_ITEMS: usize = 5000;
/// Rows per write statement.
const CHUNK: usize = 1000;
/// Rows a query returns when the caller names no limit, and at most.
const QUERY_ROWS: u32 = 200;
const QUERY_ROWS_MAX: u32 = 5000;
/// How long a query may run.
const QUERY_TIMEOUT_MS: u64 = 10_000;
/// How long temporal history is kept.
const HISTORY_RETENTION: &str = "P3650D";
/// How far ahead of the clock a put's `at_ms` may be.
const AHEAD_MS: u64 = 60_000;
const KEY_BYTES: usize = 1024;
/// The key of the nodes temporal declarations write on (deleted at once).
const SENTINEL: &str = "\u{1}schema-sentinel";
/// Labels the plugin's own modules keep in the main project under their own key schemes, and
/// the plugin's internal ones: written only by the plugin.
const RESERVED_LABELS: &[&str] = &[
    "Project", "Session", "Settings", "ProjectEntry", "Task", "Knowledge", "Note", "Rule", "Style", "BoardMessage", "Agent", "Code", "Sym", "File", "Section",
    "Source", "ContractCheck", "ContractRevision", "Audit", "Setting", "SettingSection", "Queue", "QueueConsumer", "QueueLease", "SchemaSentinel", "Routine",
    "RoutineRun", "Run", "Security", "Finding", "SecurityTraceStep", "SecurityQuickFix", "SecurityStatusChange",
];
/// Reserved records an edge may name as one of its ends, found by their `id` (`key` is their id):
/// read-only references, never created or changed by a put.
const REFERENCED_LABELS: &[&str] = &["Project", "Session", "Knowledge"];
/// Property names the plugin keeps itself (and IronGraph's `created_at`).
const RESERVED_PROPS: &[&str] = &["key", "first_ms", "last_ms", "ended_ms", "ended_reason", "created_at"];
/// Words that make a statement write, change the schema or leave the main project.
const WRITE_WORDS: &[&str] =
    &["CREATE", "MERGE", "SET", "DELETE", "DETACH", "REMOVE", "DROP", "ALTER", "LOAD", "FOREACH", "USE", "GRANT", "DENY", "REVOKE", "START", "STOP", "TERMINATE", "TRANSACTIONS", "INSERT", "COPY"];

/// The scalar type of a temporal property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Integer,
    Float,
    String,
    Boolean,
}

impl Kind {
    fn of(value: &Value) -> Option<Kind> {
        match value {
            Value::Bool(_) => Some(Kind::Boolean),
            Value::String(_) => Some(Kind::String),
            Value::Number(n) if n.is_i64() => Some(Kind::Integer),
            Value::Number(n) if n.is_f64() => Some(Kind::Float),
            _ => None,
        }
    }

    fn cypher(self) -> &'static str {
        match self {
            Kind::Integer => "INTEGER",
            Kind::Float => "FLOAT",
            Kind::String => "STRING",
            Kind::Boolean => "BOOLEAN",
        }
    }

    /// The two kinds as one: integers are taken for floats.
    fn join(self, other: Kind) -> Option<Kind> {
        match (self, other) {
            (a, b) if a == b => Some(a),
            (Kind::Integer, Kind::Float) | (Kind::Float, Kind::Integer) => Some(Kind::Float),
            _ => None,
        }
    }
}

/// What a temporal property belongs to: a node label or a relationship type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Owner {
    Label(String),
    Rel(String),
}

impl Owner {
    fn shown(&self) -> &str {
        match self {
            Owner::Label(name) | Owner::Rel(name) => name,
        }
    }
}

/// What the pre-write read found of a stored node.
#[derive(Debug, Clone, Copy)]
struct Stored {
    retired: bool,
    linked: bool,
}

pub struct GraphStore {
    graph: Arc<Graph>,
    ig: String,
    machine: MachineInfo,
    /// Labels whose key constraint this process declared.
    keyed: Mutex<HashSet<String>>,
    /// Temporal properties this process declared, with their type.
    temporal: Mutex<HashMap<(Owner, String), Kind>>,
    /// Serialises puts and retires: a batch's checks hold until it is written.
    write: Mutex<()>,
}

impl GraphStore {
    /// Open the graph in the main project and write this Mac's root node.
    pub async fn open(graph: Arc<Graph>, ig: String) -> Result<Self, RpcError> {
        let machine = tokio::task::spawn_blocking(machine_info).await.map_err(|error| RpcError::internal(format!("reading the machine's identity: {error}")))?;
        let store = Self { graph, ig, machine, keyed: Mutex::new(HashSet::new()), temporal: Mutex::new(HashMap::new()), write: Mutex::new(()) };
        let mut props = Map::new();
        props.insert("name".into(), json!(store.machine.name));
        props.insert("os".into(), json!(store.machine.os));
        let root = GraphNode { label: "Machine".into(), key: store.machine.key.clone(), props, temporal: Map::new() };
        store.put(GraphPutParams { nodes: vec![root], ..Default::default() }).await?;
        Ok(store)
    }

    /// This Mac's root node.
    pub fn machine(&self) -> &MachineInfo {
        &self.machine
    }

    /// The main project's IronGraph id.
    pub fn ig(&self) -> &str {
        &self.ig
    }

    /// The statement prefix for writes: every layer readable, so edges may join nodes of any layer.
    fn write_prefix(layer: GraphLayer) -> &'static str {
        match layer {
            GraphLayer::Observed => "USE LAYER OBSERVED, KNOWLEDGE, WORKSPACE WRITE LAYER OBSERVED",
            GraphLayer::Knowledge => "USE LAYER OBSERVED, KNOWLEDGE, WORKSPACE WRITE LAYER KNOWLEDGE",
            GraphLayer::Workspace => "USE LAYER OBSERVED, KNOWLEDGE, WORKSPACE WRITE LAYER WORKSPACE",
        }
    }

    async fn run(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig, cypher, params).await.map_err(engine_refusal)
    }

    /// `memory.graph.put`: see the module documentation.
    pub async fn put(&self, request: GraphPutParams) -> Result<GraphPutResult, RpcError> {
        let now = now_ms();
        let at = request.at_ms.unwrap_or(now);
        if at > now + AHEAD_MS {
            return Err(RpcError::invalid_params(format!("at_ms {at} is in the future (now is {now})")));
        }
        let total = request.nodes.len() + request.edges.len();
        if total > MAX_ITEMS {
            return Err(RpcError::invalid_params(format!("{total} nodes and edges in one put; at most {MAX_ITEMS}: split the batch (each part carrying its nodes' edges)")));
        }
        if total == 0 {
            return Ok(GraphPutResult::default());
        }

        // Check every name and value; the same node or edge named twice is one, later values winning.
        let mut nodes: BTreeMap<GraphRef, GraphNode> = BTreeMap::new();
        for mut node in request.nodes {
            validate_ref(&node.label, &node.key)?;
            floats_in_lists(&mut node.props);
            let what = format!("node {}:{}", node.label, node.key);
            validate_props(&what, &node.props, &node.temporal)?;
            if request.layer == GraphLayer::Knowledge && node.props.get("name").is_some_and(|name| !name.is_string()) {
                return Err(RpcError::invalid_params(format!("{what}: a knowledge node's name is a string")));
            }
            let id = GraphRef { label: node.label.clone(), key: node.key.clone() };
            match nodes.get_mut(&id) {
                Some(seen) => {
                    seen.props.extend(node.props);
                    seen.temporal.extend(node.temporal);
                }
                None => {
                    nodes.insert(id, node);
                }
            }
        }
        let mut edges: BTreeMap<(GraphRef, String, GraphRef), GraphEdge> = BTreeMap::new();
        for mut edge in request.edges {
            floats_in_lists(&mut edge.props);
            validate_end(&edge.from.label, &edge.from.key)?;
            validate_end(&edge.to.label, &edge.to.key)?;
            validate_rel(&edge.rel)?;
            let what = format!("edge {}:{} -[{}]-> {}:{}", edge.from.label, edge.from.key, edge.rel, edge.to.label, edge.to.key);
            validate_props(&what, &edge.props, &edge.temporal)?;
            let id = (edge.from.clone(), edge.rel.clone(), edge.to.clone());
            match edges.get_mut(&id) {
                Some(seen) => {
                    seen.props.extend(edge.props);
                    seen.temporal.extend(edge.temporal);
                }
                None => {
                    edges.insert(id, edge);
                }
            }
        }

        // The type of each temporal property in the batch.
        let mut kinds: BTreeMap<(Owner, String), Kind> = BTreeMap::new();
        let temporal = nodes.values().flat_map(|n| n.temporal.iter().map(move |(p, v)| (Owner::Label(n.label.clone()), p, v, format!("node {}:{}", n.label, n.key))));
        let temporal = temporal.chain(edges.values().flat_map(|e| e.temporal.iter().map(move |(p, v)| (Owner::Rel(e.rel.clone()), p, v, format!("edge -[{}]->", e.rel)))));
        for (owner, prop, value, what) in temporal {
            let kind = Kind::of(value).ok_or_else(|| RpcError::invalid_params(format!("{what}: temporal `{prop}` must be a string, number or boolean")))?;
            let entry = kinds.entry((owner.clone(), prop.clone())).or_insert(kind);
            *entry = entry.join(kind).ok_or_else(|| RpcError::invalid_params(format!("{what}: temporal `{}.{prop}` gets both {} and {} values", owner.shown(), entry.cypher(), kind.cypher())))?;
        }

        let _write = self.write.lock().await;
        // What is stored already of every node the batch names (its own and its edges' ends).
        let mut named: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for id in nodes.keys().chain(edges.keys().flat_map(|(from, _, to)| [from, to])) {
            named.entry(id.label.clone()).or_default().insert(id.key.clone());
        }
        let mut stored: HashMap<GraphRef, Stored> = HashMap::new();
        for (label, keys) in &named {
            let keys: Vec<&String> = keys.iter().collect();
            for chunk in keys.chunks(CHUNK) {
                let rows = self
                    .run(&format!("{} UNWIND $keys AS k MATCH (n:{label} {{{id}: k}}) RETURN n.{id}, n.ended_ms IS NOT NULL, EXISTS {{ (n)--() }}", layer::ALL_READ, id = key_prop(label)), params! { "keys" => chunk })
                    .await?;
                for row in &rows.rows {
                    if let Some(key) = row.first().and_then(Value::as_str) {
                        let flag = |at: usize| row.get(at).and_then(Value::as_bool).unwrap_or(false);
                        stored.insert(GraphRef { label: label.clone(), key: key.to_owned() }, Stored { retired: flag(1), linked: flag(2) });
                    }
                }
            }
        }
        // Every edge's ends exist; every node has an edge.
        let mut touched: HashSet<&GraphRef> = HashSet::new();
        for (from, rel, to) in edges.keys() {
            for end in [from, to] {
                if !nodes.contains_key(end) && !stored.contains_key(end) {
                    return Err(RpcError::invalid_params(format!(
                        "edge {}:{} -[{rel}]-> {}:{} names {}:{}, which is neither in this batch nor stored",
                        from.label, from.key, to.label, to.key, end.label, end.key
                    )));
                }
                touched.insert(end);
            }
        }
        let lonely: Vec<&GraphRef> = nodes.keys().filter(|id| id.label != "Machine" && !touched.contains(id) && !stored.get(*id).is_some_and(|s| s.linked)).collect();
        if let Some(first) = lonely.first() {
            let more = if lonely.len() > 1 { format!(" (and {} more)", lonely.len() - 1) } else { String::new() };
            return Err(RpcError::invalid_params(format!(
                "node {}:{}{more} has no edge in this batch and none stored: nothing is kept unconnected, so send it with the edge that links it (only Machine stands alone)",
                first.label, first.key
            )));
        }

        // Schema: each label's key constraint, each temporal property's declaration.
        for label in named.keys().filter(|label| !REFERENCED_LABELS.contains(&label.as_str())) {
            self.ensure_keyed(label).await?;
        }
        let mut declared: HashMap<(Owner, String), Kind> = HashMap::new();
        for ((owner, prop), kind) in &kinds {
            let sample = nodes
                .values()
                .filter(|n| matches!(owner, Owner::Label(l) if *l == n.label))
                .find_map(|n| n.temporal.get(prop))
                .or_else(|| edges.values().filter(|e| matches!(owner, Owner::Rel(r) if *r == e.rel)).find_map(|e| e.temporal.get(prop)))
                .cloned()
                .unwrap_or(Value::Null);
            let kind = self.ensure_temporal(owner, prop, *kind, &sample).await?;
            declared.insert((owner.clone(), prop.clone()), kind);
        }
        let sample = |owner: &Owner, prop: &str, value: &Value| -> Value {
            match (declared.get(&(owner.clone(), prop.to_owned())), value.as_i64()) {
                (Some(Kind::Float), Some(int)) => json!(int as f64),
                _ => value.clone(),
            }
        };

        let prefix = Self::write_prefix(request.layer);
        // Retired nodes put again are back.
        let mut revived: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for id in nodes.keys().filter(|id| stored.get(*id).is_some_and(|s| s.retired)) {
            revived.entry(id.label.as_str()).or_default().push(id.key.as_str());
        }
        for (label, keys) in revived {
            self.run(&format!("{prefix} UNWIND $keys AS k MATCH (n:{label} {{key: k}}) SET n.ended_ms = null, n.ended_reason = null"), params! { "keys" => keys }).await?;
            self.run(&format!("{prefix} UNWIND $keys AS k MATCH (n:{label} {{key: k}}) WHERE n.state = 'gone' SET n.state = null"), params! { "keys" => keys }).await?;
        }

        // Nodes, one statement per label; then each temporal property, one statement per label.
        let mut groups: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        let mut samples: BTreeMap<(&str, &String), Vec<Value>> = BTreeMap::new();
        let mut created: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        for (id, node) in &nodes {
            groups.entry(node.label.as_str()).or_default().push(json!({"key": node.key, "props": node.props}));
            let owner = Owner::Label(node.label.clone());
            for (prop, value) in &node.temporal {
                samples.entry((node.label.as_str(), prop)).or_default().push(json!({"key": node.key, "v": sample(&owner, prop, value)}));
            }
            if request.layer == GraphLayer::Knowledge && !stored.contains_key(id) {
                let name = node.props.get("name").and_then(Value::as_str).unwrap_or(&node.key);
                created.entry(node.label.as_str()).or_default().push(json!({"key": node.key, "name": name}));
            }
        }
        // A knowledge node is created with its name (IronGraph refuses one without).
        for (label, rows) in created {
            for chunk in rows.chunks(CHUNK) {
                self.run(&format!("{prefix} UNWIND $rows AS r CREATE (n:{label} {{key: r.key, name: r.name}})"), params! { "rows" => chunk }).await?;
            }
        }
        for (label, rows) in &groups {
            let statement = format!("{prefix} UNWIND $rows AS r MERGE (n:{label} {{key: r.key}}) SET n += r.props, {}", span_sets("n"));
            for chunk in rows.chunks(CHUNK) {
                self.run(&statement, params! { "rows" => chunk, "at" => at }).await?;
            }
        }
        for ((label, prop), rows) in &samples {
            let statement = format!("{prefix} UNWIND $rows AS r MATCH (n:{label} {{key: r.key}}) {}", sample_set("n", prop));
            for chunk in rows.chunks(CHUNK) {
                self.run(&statement, params! { "rows" => chunk, "at" => at }).await?;
            }
        }

        // Edges, one statement per (from label, type, to label); then each temporal property.
        let mut groups: BTreeMap<(&str, &str, &str), Vec<Value>> = BTreeMap::new();
        let mut samples: BTreeMap<(&str, &str, &str, &String), Vec<Value>> = BTreeMap::new();
        for edge in edges.values() {
            let group = (edge.from.label.as_str(), edge.rel.as_str(), edge.to.label.as_str());
            groups.entry(group).or_default().push(json!({"from": edge.from.key, "to": edge.to.key, "props": edge.props}));
            let owner = Owner::Rel(edge.rel.clone());
            for (prop, value) in &edge.temporal {
                samples.entry((group.0, group.1, group.2, prop)).or_default().push(json!({"from": edge.from.key, "to": edge.to.key, "v": sample(&owner, prop, value)}));
            }
        }
        for ((from, rel, to), rows) in &groups {
            let statement = format!(
                "{prefix} UNWIND $rows AS r MATCH (a:{from} {{{}: r.from}}) MATCH (b:{to} {{{}: r.to}}) MERGE (a)-[e:{rel}]->(b) SET e += r.props, {}",
                key_prop(from),
                key_prop(to),
                span_sets("e")
            );
            for chunk in rows.chunks(CHUNK) {
                self.run(&statement, params! { "rows" => chunk, "at" => at }).await?;
            }
        }
        for ((from, rel, to, prop), rows) in &samples {
            let statement =
                format!("{prefix} UNWIND $rows AS r MATCH (a:{from} {{{}: r.from}}) MATCH (b:{to} {{{}: r.to}}) MATCH (a)-[e:{rel}]->(b) {}", key_prop(from), key_prop(to), sample_set("e", prop));
            for chunk in rows.chunks(CHUNK) {
                self.run(&statement, params! { "rows" => chunk, "at" => at }).await?;
            }
        }
        Ok(GraphPutResult { nodes: nodes.len() as u32, edges: edges.len() as u32 })
    }

    /// `memory.graph.retire`: `ended_ms`, `ended_reason` and `state: 'gone'` on nodes not
    /// already retired.
    pub async fn retire(&self, request: GraphRetireParams) -> Result<u32, RpcError> {
        if request.refs.len() > MAX_ITEMS {
            return Err(RpcError::invalid_params(format!("{} refs in one retire; at most {MAX_ITEMS}", request.refs.len())));
        }
        let at = request.at_ms.unwrap_or_else(now_ms);
        let mut by_label: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for id in &request.refs {
            validate_ref(&id.label, &id.key)?;
            by_label.entry(id.label.clone()).or_default().insert(id.key.clone());
        }
        let _write = self.write.lock().await;
        let mut retired = 0;
        for (label, keys) in by_label {
            let keys: Vec<String> = keys.into_iter().collect();
            for chunk in keys.chunks(CHUNK) {
                let rows = self
                    .run(
                        &format!(
                            "{} UNWIND $keys AS k MATCH (n:{label} {{key: k}}) WHERE n.ended_ms IS NULL SET n.ended_ms = $at, n.ended_reason = $reason, n.state = 'gone' RETURN count(n)",
                            Self::write_prefix(GraphLayer::Observed)
                        ),
                        params! { "keys" => chunk, "at" => at, "reason" => request.reason },
                    )
                    .await?;
                retired += rows.rows.first().and_then(|row| row.first()).and_then(Value::as_u64).unwrap_or(0) as u32;
            }
        }
        Ok(retired)
    }

    /// `memory.graph.query`: one read-only statement over every layer, time-limited.
    pub async fn query(&self, request: GraphQueryParams) -> Result<GraphQueryResult, RpcError> {
        let limit = request.limit.unwrap_or(QUERY_ROWS).clamp(1, QUERY_ROWS_MAX) as usize;
        let cypher = request.cypher.trim().trim_end_matches(';').trim();
        if cypher.is_empty() {
            return Err(RpcError::invalid_params("the query is empty"));
        }
        check_read_only(cypher)?;
        let statement = format!("{} {cypher}", layer::ALL_READ);
        self.graph.check_read_only(&self.ig, &statement).await?;
        let rows = self.graph.run_timed(&self.ig, &statement, request.params, QUERY_TIMEOUT_MS).await.map_err(|error| {
            let lower = error.message.to_lowercase();
            if lower.contains("timeout") || lower.contains("timed out") || lower.contains("deadline") {
                RpcError::invalid_params(format!("the query ran longer than {} s: narrow it (a label, a key, a LIMIT)", QUERY_TIMEOUT_MS / 1000))
            } else if lower.contains("query_syntax") || lower.contains("query_type") {
                RpcError::invalid_params(error.message)
            } else {
                error
            }
        })?;
        let truncated = rows.rows.len() > limit;
        let mut out = rows.rows;
        out.truncate(limit);
        Ok(GraphQueryResult { columns: rows.columns, rows: out, truncated })
    }

    /// Declare `label`'s key unique (once per process); a label whose stored keys repeat gets a
    /// plain index instead.
    pub async fn ensure_keyed(&self, label: &str) -> Result<(), RpcError> {
        if self.keyed.lock().await.contains(label) {
            return Ok(());
        }
        let name = format!("graph_key_{label}");
        match self.graph.run(&self.ig, &format!("CREATE CONSTRAINT {name} FOR (n:{label}) REQUIRE n.key IS UNIQUE"), Map::new()).await {
            Ok(_) => {}
            Err(error) if error.message.contains("already exists") => {}
            Err(error) => {
                tracing::warn!(label, error = %error.message, "no unique key constraint; an index instead");
                self.graph.ensure_index(&self.ig, &format!("graph_keyidx_{label}"), label, "key").await?;
            }
        }
        self.keyed.lock().await.insert(label.to_owned());
        Ok(())
    }

    /// Declare a temporal property (see the module documentation). Returns the type it keeps,
    /// which is `float` for integers sent to a float property.
    async fn ensure_temporal(&self, owner: &Owner, prop: &str, kind: Kind, sample: &Value) -> Result<Kind, RpcError> {
        let id = (owner.clone(), prop.to_owned());
        if let Some(known) = self.temporal.lock().await.get(&id).copied() {
            return known.join(kind).filter(|joined| *joined == known).ok_or_else(|| type_clash(owner, prop, known, kind));
        }
        let prefix = Self::write_prefix(GraphLayer::Observed);
        let sentinel = params! { "s" => SENTINEL, "v" => sample };
        let (seed, alter, clear) = match owner {
            Owner::Label(label) => (
                format!("{prefix} MERGE (s:{label} {{key: $s}}) SET s.`{prop}` = $v"),
                format!("ALTER NODE PROPERTY {label}.{prop} SET TEMPORAL {{kind}} RETENTION duration('{HISTORY_RETENTION}')"),
                format!("{prefix} MATCH (s:{label} {{key: $s}}) DETACH DELETE s"),
            ),
            Owner::Rel(rel) => (
                format!("{prefix} MERGE (s:SchemaSentinel {{key: $s}}) MERGE (s)-[e:{rel}]->(s) SET e.`{prop}` = $v"),
                format!("ALTER RELATIONSHIP PROPERTY {rel}.{prop} SET TEMPORAL {{kind}} RETENTION duration('{HISTORY_RETENTION}')"),
                format!("{prefix} MATCH (s:SchemaSentinel {{key: $s}}) DETACH DELETE s"),
            ),
        };
        self.run(&seed, sentinel.clone()).await?;
        let mut kept = kind;
        let mut outcome = self.graph.run(&self.ig, &alter.replace("{kind}", kind.cypher()), Map::new()).await;
        if let Err(error) = &outcome
            && error.message.contains("cannot change scalar type")
            && kind == Kind::Integer
        {
            kept = Kind::Float;
            outcome = self.graph.run(&self.ig, &alter.replace("{kind}", Kind::Float.cypher()), Map::new()).await;
        }
        self.run(&clear, sentinel).await?;
        match outcome {
            Ok(_) => {
                self.temporal.lock().await.insert(id, kept);
                Ok(kept)
            }
            Err(error) if error.message.contains("cannot change scalar type") => {
                Err(RpcError::invalid_params(format!("temporal `{}.{prop}` keeps a history of another type than {}", owner.shown(), kind.cypher())))
            }
            Err(error) => Err(error),
        }
    }
}

/// `SET` items that keep `first_ms` the earliest and `last_ms` the latest time seen.
fn span_sets(var: &str) -> String {
    format!(
        "{var}.first_ms = CASE WHEN {var}.first_ms IS NULL OR {var}.first_ms > $at THEN $at ELSE {var}.first_ms END, \
         {var}.last_ms = CASE WHEN {var}.last_ms IS NULL OR {var}.last_ms < $at THEN $at ELSE {var}.last_ms END"
    )
}

/// A temporal sample from the row's `v`, written only when it differs from the current value:
/// a history of changes, and a repeated put adds nothing.
fn sample_set(var: &str, prop: &str) -> String {
    format!("WITH {var}, r WHERE {var}.`{prop}` IS NULL OR {var}.`{prop}` <> r.v SET {var}.`{prop}` = r.v AT TIME datetime.fromepochmillis($at)")
}

fn type_clash(owner: &Owner, prop: &str, known: Kind, got: Kind) -> RpcError {
    RpcError::invalid_params(format!("temporal `{}.{prop}` keeps {} values; got {}", owner.shown(), known.cypher(), got.cypher()))
}

/// An engine refusal of a value (an old sample, a bad list) is the caller's, not the plugin's.
fn engine_refusal(error: RpcError) -> RpcError {
    if error.message.contains("RETENTION_EXPIRED") {
        return RpcError::invalid_params(format!("at_ms is older than the history kept ({HISTORY_RETENTION}): {}", error.message));
    }
    if error.message.contains("InvalidPropertyType") || error.message.contains("wrong scalar type") || error.message.contains("INVALID_DATA") {
        return RpcError::invalid_params(error.message);
    }
    error
}

/// A list of integers and floats becomes a list of floats (IronGraph keeps one scalar type per list).
fn floats_in_lists(props: &mut Map<String, Value>) {
    for value in props.values_mut() {
        if let Value::Array(items) = value
            && items.iter().any(|item| item.is_f64())
            && items.iter().all(Value::is_number)
        {
            for item in items.iter_mut() {
                if let Some(float) = item.as_f64() {
                    *item = json!(float);
                }
            }
        }
    }
}

/// A label (CamelCase, not one of the plugin's own) and a key.
fn validate_ref(label: &str, key: &str) -> Result<(), RpcError> {
    let camel = label.len() <= 64 && label.starts_with(|c: char| c.is_ascii_uppercase()) && label.chars().all(|c| c.is_ascii_alphanumeric());
    if !camel {
        return Err(RpcError::invalid_params(format!("label `{label}`: CamelCase, an ASCII capital then letters and digits, at most 64")));
    }
    if RESERVED_LABELS.contains(&label) {
        return Err(RpcError::invalid_params(format!("label `{label}` is kept by the memory plugin itself (memory.app.*, memory.task.*, …), not through memory.graph.put")));
    }
    if key.is_empty() || key.len() > KEY_BYTES || key == SENTINEL {
        return Err(RpcError::invalid_params(format!("{label}: a key is 1 to {KEY_BYTES} bytes")));
    }
    Ok(())
}

/// An edge's end: a label and key as [`validate_ref`], or a reference to a `Project`, `Session` or
/// `Knowledge` item
/// by its id.
fn validate_end(label: &str, key: &str) -> Result<(), RpcError> {
    if !REFERENCED_LABELS.contains(&label) {
        return validate_ref(label, key);
    }
    if key.is_empty() || key.len() > KEY_BYTES {
        return Err(RpcError::invalid_params(format!("{label}: an id is 1 to {KEY_BYTES} bytes")));
    }
    Ok(())
}

/// The property a label's nodes are found by: `id` for [`REFERENCED_LABELS`], `key` otherwise.
fn key_prop(label: &str) -> &'static str {
    if REFERENCED_LABELS.contains(&label) { "id" } else { "key" }
}

/// A relationship type: UPPER_SNAKE.
fn validate_rel(rel: &str) -> Result<(), RpcError> {
    let ok = rel.len() <= 64 && rel.starts_with(|c: char| c.is_ascii_uppercase()) && rel.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if ok { Ok(()) } else { Err(RpcError::invalid_params(format!("relationship type `{rel}`: UPPER_SNAKE, an ASCII capital then capitals, digits and `_`, at most 64"))) }
}

/// A property name: snake_case.
fn validate_prop_name(what: &str, name: &str) -> Result<(), RpcError> {
    let ok = name.len() <= 64 && name.starts_with(|c: char| c.is_ascii_lowercase()) && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(RpcError::invalid_params(format!("{what}: property `{name}`: snake_case, a lowercase letter then lowercase letters, digits and `_`, at most 64")));
    }
    if RESERVED_PROPS.contains(&name) {
        return Err(RpcError::invalid_params(format!("{what}: property `{name}` is kept by the plugin")));
    }
    Ok(())
}

/// Ordinary and temporal properties: names, and values: scalars, or lists of one scalar type
/// (structure is modelled as nodes and edges, never as maps or JSON text).
fn validate_props(what: &str, props: &Map<String, Value>, temporal: &Map<String, Value>) -> Result<(), RpcError> {
    if props.len() + temporal.len() > 200 {
        return Err(RpcError::invalid_params(format!("{what}: more than 200 properties")));
    }
    for (name, value) in props {
        validate_prop_name(what, name)?;
        if value.is_object() {
            return Err(RpcError::invalid_params(format!("{what}: `{name}` is a map; properties are scalars or lists of scalars: model structure as nodes and edges")));
        }
        if let Value::Array(items) = value {
            let mut kinds = items.iter().map(|item| Kind::of(item).ok_or(()).map(|kind| if kind == Kind::Float { Kind::Integer } else { kind }));
            let first = kinds.next();
            if first.is_some_and(|first| first.is_err() || kinds.any(|kind| kind != first)) {
                return Err(RpcError::invalid_params(format!("{what}: list `{name}` must hold strings, numbers or booleans, all of one type")));
            }
        }
    }
    for (name, value) in temporal {
        validate_prop_name(what, name)?;
        if props.contains_key(name) {
            return Err(RpcError::invalid_params(format!("{what}: `{name}` is under both props and temporal")));
        }
        if value.as_u64().is_some_and(|n| n > i64::MAX as u64) {
            return Err(RpcError::invalid_params(format!("{what}: temporal `{name}` is larger than a 64-bit integer")));
        }
    }
    Ok(())
}

/// Refuse a statement that could write, change the schema, name another project or hold more
/// than one statement. Words inside strings, comments and backticks, after `.`, `$` or `:`, or
/// before `:` (map keys) are names, not clauses. The engine checks again (`CHECK READ ONLY`).
pub fn check_read_only(cypher: &str) -> Result<(), RpcError> {
    let chars: Vec<char> = cypher.chars().collect();
    let mut i = 0;
    // The last significant character before the current word.
    let mut before = ' ';
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' => {
                i += 1;
                while i < chars.len() && chars[i] != c {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                before = c;
            }
            '`' => {
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    i += 1;
                }
                i += 1;
                before = '`';
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            ';' => return Err(RpcError::invalid_params("one statement per query (no `;`)")),
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect::<String>().to_uppercase();
                let after = chars[i..].iter().find(|c| !c.is_whitespace()).copied().unwrap_or(' ');
                let is_name = matches!(before, '.' | '$' | ':') || after == ':';
                if !is_name && WRITE_WORDS.contains(&word.as_str()) {
                    return Err(RpcError::invalid_params(format!(
                        "memory.graph.query is read-only: `{word}` is refused (write through memory.graph.put; the project and layers are the plugin's)"
                    )));
                }
                before = 'a';
            }
            c if c.is_whitespace() => i += 1,
            c => {
                before = c;
                i += 1;
            }
        }
    }
    Ok(())
}

/// This Mac's identity: the hardware UUID (`IOPlatformUUID`, else `kern.uuid`), its computer name
/// and macOS version.
fn machine_info() -> MachineInfo {
    let output = |program: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(program).args(args).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (out.status.success() && !text.is_empty()).then_some(text)
    };
    let key = output("/usr/sbin/ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])
        .and_then(|text| text.lines().find(|line| line.contains("\"IOPlatformUUID\"")).and_then(|line| line.rsplit('"').nth(1).map(str::to_owned)))
        .filter(|uuid| !uuid.is_empty())
        .or_else(|| output("/usr/sbin/sysctl", &["-n", "kern.uuid"]))
        .or_else(|| output("/bin/hostname", &[]))
        .unwrap_or_else(|| "unknown-machine".into());
    let name = output("/usr/sbin/scutil", &["--get", "ComputerName"]).or_else(|| output("/bin/hostname", &[])).unwrap_or_default();
    let os = match (output("/usr/bin/sw_vers", &["-productName"]), output("/usr/bin/sw_vers", &["-productVersion"]), output("/usr/bin/sw_vers", &["-buildVersion"])) {
        (Some(product), Some(version), Some(build)) => format!("{product} {version} ({build})"),
        (Some(product), Some(version), None) => format!("{product} {version}"),
        _ => std::env::consts::OS.to_owned(),
    };
    MachineInfo { key, name, os }
}
