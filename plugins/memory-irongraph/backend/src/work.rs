//! Work state in the WORKSPACE layer: tasks (hierarchy, contracts, dependencies, touched code),
//! the board, agents, rules (CORTEX.md and learned), user style and run records.
//!
//! ```text
//! (:Task {id, project_id, parent_id, title, brief, plan, status*, assignee, depends_on, touches,
//!         result, session_id, effort, contract_hash, created_ms, updated_ms})
//!     -[:IN]->(root)                  a task without a parent; a subtask hangs from its parent
//! (:Task)-[:HAS_SUBTASK]->(:Task)   (:Task)-[:DEPENDS_ON]->(:Task)
//! (:Task)-[:HAS_CHECK {position}]->(:ContractCheck {name, kind, spec, expect, position, task_id})
//! (:Task)-[:REVISED]->(:ContractRevision {old_hash, new_hash, reason, proposed_by, accepted_by, ts_ms})
//! (:Task)-[:TOUCHES {ref, position}]->(:Code)
//! (:Agent {name, role, charter, status*, session_id, task_id, workspace, tasks_done, tasks_failed,
//!          updated_ms, mode})-[:IN]->(root)   -[:WORKS_ON]->(:Task)   (its `task_id`)
//! (:Rule {id, scope, text, source, section, applies_to, position, updated_ms})-[:IN]->(root)
//!     -[:CONSTRAINS {ref, position}]->(:Code)
//! (:Style {id, area, text, scope, confidence, evidence, updated_ms})-[:IN]->(root)
//! (:Run {id, outcome, summary, touched, ts_ms, metric_*})-[:IN]->(root)   -[:FOR_TASK]->(:Task)
//! (:Code)-[:MODIFIED_IN]->(:Run)
//! stream `board`                    the board's messages, in the order posted (kept 90 days)
//! ```
//!
//! `root` is the repository's code root (`Code:Repo {sid: 'repo:'}`, made here when no sync has
//! made it yet), or in the global scope (the main project) this Mac's `Machine`. Every node and
//! edge keeps `first_ms` and `last_ms`. Nothing is deleted: a record that is gone is retired
//! (`ended_ms`, `ended_reason`, `state: 'gone'`), an edge that no longer holds gets `ended_ms`,
//! and every read skips both; writing a record or edge again clears its retirement.
//!
//! `*` marks a temporal property (IronGraph keeps its history): declared once per process and
//! project (the values stored before the declaration are written again as its first samples),
//! written `AT TIME` only when the value changes. Board messages are a history, not records: an
//! earlier version's `BoardMessage` nodes are moved into the stream once and retired.

use crate::db::{Graph, layer};
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{
    AgentRecord, BoardListParams, contract_change, BoardMessage, BoardPostParams, Contract, ContractCheck, ContractRevision, Rule, RunRecordParams, StyleEntry, Task, TaskCreateParams,
    TaskUpdateParams,
};
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

/// The work store of one IronGraph project (a repository, or the global scope).
pub struct WorkStore {
    pub graph: Arc<Graph>,
    pub ig_id: String,
    /// Frame project id, or `global`.
    pub scope: String,
    indexes: Mutex<Vec<&'static str>>,
}

fn s(row: &[Value], i: usize) -> String {
    row.get(i).and_then(|v| v.as_str()).unwrap_or("").to_owned()
}

fn opt_s(row: &[Value], i: usize) -> Option<String> {
    row.get(i).and_then(|v| v.as_str()).map(str::to_owned)
}

fn n(row: &[Value], i: usize) -> u64 {
    row.get(i).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0)
}

fn list(row: &[Value], i: usize) -> Vec<String> {
    row.get(i).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

const TASK_COLS: &str = "t.id, t.project_id, t.parent_id, t.title, t.brief, t.plan, t.status, t.assignee, t.depends_on, t.touches, t.result, t.session_id, t.created_ms, t.updated_ms, t.contract_hash, t.contract_revisions, t.effort";

/// How long the history of a temporal property is kept (as the graph API keeps it).
const HISTORY_RETENTION: &str = "P3650D";
/// The prefix of the key of the node a temporal declaration writes on (one per call, deleted at
/// once, and born retired so no read ever lists it).
const SENTINEL: &str = "\u{1}schema-sentinel";
/// The stream the board's messages are kept in, and for how long.
const BOARD_TOPIC: &str = "board";
const BOARD_RETENTION_DAYS: u32 = 90;
/// Bytes fetched per page of the board's stream.
const BOARD_PAGE_BYTES: usize = 8 << 20;
/// Rows per write statement of a migration.
const CHUNK: usize = 500;

/// The temporal property of each work label: (label, key property, property, write prefix, read prefix).
const TEMPORAL: &[(&str, &str, &str, &str, &str)] = &[
    ("Task", "id", "status", layer::WORKSPACE_WRITE, layer::WORKSPACE_READ),
    ("Agent", "name", "status", layer::WORKSPACE_WRITE, layer::WORKSPACE_READ),
    ("Routine", "id", "state", layer::WORKSPACE_WRITE, layer::WORKSPACE_READ),
    ("Knowledge", "id", "state", layer::KNOWLEDGE_WRITE, layer::KNOWLEDGE_READ),
];

/// What this process already did per IronGraph project (`<project>|<what>`): temporal
/// declarations (with whether they took), roots made, boards moved into their stream. Work
/// stores of projects that are not open are made per call, so this is process-wide.
fn done() -> &'static std::sync::Mutex<HashMap<String, bool>> {
    static DONE: OnceLock<std::sync::Mutex<HashMap<String, bool>>> = OnceLock::new();
    DONE.get_or_init(Default::default)
}

fn done_get(key: &str) -> Option<bool> {
    done().lock().ok().and_then(|map| map.get(key).copied())
}

fn done_set(key: String, value: bool) {
    if let Ok(mut map) = done().lock() {
        map.insert(key, value);
    }
}

/// `SET` items that keep `first_ms` the first write and `last_ms` the latest (needs `$now`).
pub(crate) fn span(var: &str) -> String {
    format!("{var}.first_ms = CASE WHEN {var}.first_ms IS NULL THEN $now ELSE {var}.first_ms END, {var}.last_ms = $now")
}

/// `SET` items that bring a retired node back (it is written again).
pub(crate) fn revive(var: &str) -> String {
    format!("{var}.ended_ms = null, {var}.ended_reason = null")
}

/// A board message from its stream record (`{at_ms, value}`).
fn board_record(bytes: &[u8]) -> Option<BoardMessage> {
    let mut envelope: Value = serde_json::from_slice(bytes).ok()?;
    serde_json::from_value(envelope.get_mut("value")?.take()).ok()
}

fn board_envelope(message: &BoardMessage) -> Result<Vec<u8>, RpcError> {
    serde_json::to_vec(&json!({"at_ms": message.ts_ms, "value": message})).map_err(|error| RpcError::internal(error.to_string()))
}

impl WorkStore {
    pub fn new(graph: Arc<Graph>, ig_id: String, scope: String) -> Self {
        Self { graph, ig_id, scope, indexes: Mutex::new(Vec::new()) }
    }

    async fn run(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    async fn index(&self, name: &'static str, label: &str, property: &str) -> Result<(), RpcError> {
        let mut done = self.indexes.lock().await;
        if done.contains(&name) {
            return Ok(());
        }
        self.graph.ensure_index(&self.ig_id, name, label, property).await?;
        done.push(name);
        Ok(())
    }

    // ---- the root, temporal properties --------------------------------------------------------

    /// The clause that binds `root`: the repository's code root, or the Machine in the global scope.
    pub(crate) fn root_match(&self) -> &'static str {
        if self.scope == "global" { "MATCH (root:Machine)" } else { "MATCH (root:Repo {sid: 'repo:'})" }
    }

    /// Make the repository's code root when no sync has made it yet (keyed as the code tree keys
    /// it, so the tree's first sync merges onto it). The Machine is written at the start.
    async fn ensure_root(&self) -> Result<(), RpcError> {
        if self.scope == "global" {
            return Ok(());
        }
        let key = format!("{}|root", self.ig_id);
        if done_get(&key).is_some() {
            return Ok(());
        }
        let found = self.run(&format!("{} MATCH (r:Repo {{sid: 'repo:'}}) RETURN r.sid LIMIT 1", layer::OBSERVED_READ), Map::new()).await?;
        if found.rows.is_empty() {
            self.run(
                &format!(
                    "{} MERGE (r:Code:Repo {{sid: 'repo:'}}) ON CREATE SET r.kind = 'repo', r.name = $name, r.qname = '', r.path = '' SET {}",
                    layer::OBSERVED_WRITE,
                    span("r")
                ),
                params! { "name" => self.scope, "now" => now_ms() },
            )
            .await?;
        }
        done_set(key, true);
        Ok(())
    }

    /// Link the nodes `matched` binds as `w` (in `prefix`'s write layer) to the root: `-[:IN]->`.
    pub(crate) async fn link_root(&self, prefix: &str, matched: &str, mut p: Map<String, Value>) -> Result<(), RpcError> {
        self.ensure_root().await?;
        p.insert("now".into(), json!(now_ms()));
        self.run(&format!("{prefix} {matched} {} MERGE (w)-[e:IN]->(root) SET {}", self.root_match(), span("e")), p).await?;
        Ok(())
    }

    /// Declare a label's temporal property in this project, once per process. The declaration
    /// takes effect only on a property some write made (a sentinel node makes it) and turns the
    /// ordinary values into nothing, so the values stored before it are read first and written
    /// again as samples. False when IronGraph refused it: the property is then written plainly.
    async fn ensure_temporal(&self, label: &str) -> Result<bool, RpcError> {
        let Some(&(label, key_prop, prop, write, read)) = TEMPORAL.iter().find(|t| t.0 == label) else { return Ok(false) };
        let cache = format!("{}|temporal|{label}.{prop}", self.ig_id);
        if let Some(declared) = done_get(&cache) {
            return Ok(declared);
        }
        let rows = self.run(&format!("{read} MATCH (n:{label}) WHERE n.{prop} IS NOT NULL RETURN n.{key_prop}, n.{prop}"), Map::new()).await?;
        let current: Vec<Value> = rows
            .rows
            .iter()
            .filter_map(|row| {
                let key = row.first()?.as_str()?;
                let value = row.get(1)?.as_str()?;
                (!key.starts_with(SENTINEL)).then(|| json!({"k": key, "v": value}))
            })
            .collect();
        // Each call writes its own sentinel, so a concurrent first declaration of the same
        // property cannot delete it before this one's ALTER runs.
        let sentinel = format!("{SENTINEL}-{}", uuid::Uuid::new_v4().simple());
        let name = if label == "Knowledge" { ", name: $s" } else { "" };
        self.run(&format!("{write} MERGE (s:{label} {{{key_prop}: $s{name}}}) SET s.{prop} = $v, s.ended_ms = 0"), params! { "s" => sentinel, "v" => "gone" }).await?;
        // Earlier versions indexed `Knowledge.state` for equality; a temporal property has no
        // such index, so the old one goes before the declaration.
        if label == "Knowledge"
            && let Err(error) = self.graph.run(&self.ig_id, "DROP INDEX knowledge_state IF EXISTS", Map::new()).await
        {
            tracing::warn!(error = %error.message, "the old knowledge_state index could not be dropped");
        }
        let outcome = self.graph.run(&self.ig_id, &format!("ALTER NODE PROPERTY {label}.{prop} SET TEMPORAL STRING RETENTION duration('{HISTORY_RETENTION}')"), Map::new()).await;
        self.run(&format!("{write} MATCH (s:{label} {{{key_prop}: $s}}) DETACH DELETE s"), params! { "s" => sentinel }).await?;
        let declared = match outcome {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(label, prop, error = %error.message, "a temporal property could not be declared; it is written without history");
                false
            }
        };
        if declared {
            let at = now_ms();
            for chunk in current.chunks(CHUNK) {
                self.run(
                    &format!("{write} UNWIND $rows AS r MATCH (n:{label} {{{key_prop}: r.k}}) WITH n, r WHERE n.{prop} IS NULL OR n.{prop} <> r.v SET n.{prop} = r.v AT TIME datetime.fromepochmillis($at)"),
                    params! { "rows" => chunk, "at" => at },
                )
                .await?;
            }
        }
        done_set(cache, declared);
        Ok(declared)
    }

    /// Write the temporal property of `label` on the node keyed `key`: a sample at the current time,
    /// only when the value changes (`when_absent`: only when the node has none yet).
    pub(crate) async fn set_temporal(&self, label: &str, key: &str, value: &str, when_absent: bool) -> Result<(), RpcError> {
        let Some(&(label, key_prop, prop, write, _)) = TEMPORAL.iter().find(|t| t.0 == label) else {
            return Err(RpcError::internal(format!("{label} has no temporal property")));
        };
        let guard = if when_absent { format!("n.{prop} IS NULL") } else { format!("n.{prop} IS NULL OR n.{prop} <> $v") };
        let at_time = if self.ensure_temporal(label).await? { " AT TIME datetime.fromepochmillis($at)" } else { "" };
        self.run(&format!("{write} MATCH (n:{label} {{{key_prop}: $key}}) WITH n WHERE {guard} SET n.{prop} = $v{at_time}"), params! { "key" => key, "v" => value, "at" => now_ms() }).await?;
        Ok(())
    }

    // ---- code links -----------------------------------------------------------------------------

    /// Link `from` (a work node matched by `match_clause` as `w`) to the live code nodes named by
    /// `refs` (sids, repository paths, qualified or unique bare names). An edge kept from before
    /// (same reference, same node) is opened again.
    async fn link_code(&self, match_clause: &str, key: &Map<String, Value>, rel: &str, refs: &[String]) -> Result<(), RpcError> {
        for (position, reference) in refs.iter().enumerate() {
            let target = self.code_sid(reference).await?;
            let Some(target) = target else { continue };
            let mut p = key.clone();
            p.insert("target".into(), json!(target));
            p.insert("ref".into(), json!(reference));
            p.insert("position".into(), json!(position));
            p.insert("now".into(), json!(now_ms()));
            self.run(
                &format!(
                    "{} {match_clause} MATCH (c:Code {{sid: $target}}) WHERE c.ended_ms IS NULL MERGE (w)-[r:{rel} {{ref: $ref}}]->(c) SET r.position = $position, r.ended_ms = null, {}",
                    layer::WORKSPACE_WRITE,
                    span("r")
                ),
                p,
            )
            .await?;
        }
        Ok(())
    }

    /// Mark the live `rel` edges from the node `match_clause` binds as `w` as ended.
    async fn end_edges(&self, match_clause: &str, key: Map<String, Value>, rel: &str) -> Result<(), RpcError> {
        let mut p = key;
        p.insert("now".into(), json!(now_ms()));
        self.run(&format!("{} {match_clause} MATCH (w)-[r:{rel}]->() WHERE r.ended_ms IS NULL SET r.ended_ms = $now, r.last_ms = $now", layer::WORKSPACE_WRITE), p).await?;
        Ok(())
    }

    /// The sid of the live code node a reference names, if exactly one.
    pub async fn code_sid(&self, reference: &str) -> Result<Option<String>, RpcError> {
        let attempts = [
            ("MATCH (c:Code {sid: $v}) WHERE c.ended_ms IS NULL RETURN c.sid", reference.to_owned()),
            ("MATCH (c:File {sid: $v}) WHERE c.ended_ms IS NULL RETURN c.sid", format!("file:{}", reference.trim_start_matches("./"))),
            ("MATCH (c:Sym {qname: $v}) WHERE c.ended_ms IS NULL RETURN c.sid", reference.to_owned()),
            ("MATCH (c:Sym {name: $v}) WHERE c.ended_ms IS NULL RETURN c.sid", reference.to_owned()),
        ];
        for (cypher, value) in attempts {
            let rows = self.run(&format!("{} {cypher}", layer::OBSERVED_READ), params! { "v" => value }).await?;
            if rows.rows.len() == 1 {
                return Ok(Some(s(&rows.rows[0], 0)));
            }
            if rows.rows.len() > 1 {
                return Ok(None);
            }
        }
        Ok(None)
    }

    // ---- tasks ---------------------------------------------------------------------------------

    pub async fn task_create(&self, p: TaskCreateParams) -> Result<Task, RpcError> {
        let id = new_id("task");
        let now = now_ms();
        if let Some(parent) = &p.parent_id
            && self.task_row(parent).await?.is_none()
        {
            return Err(RpcError::not_found(format!("parent task {parent} not found")));
        }
        self.run(
            &format!(
                "{} CREATE (t:Task {{id: $id, name: $title, project_id: $project, parent_id: $parent, title: $title, brief: $brief, plan: $plan, assignee: $assignee, depends_on: $depends, touches: $touches, effort: $effort, created_ms: $now, updated_ms: $now, first_ms: $now, last_ms: $now}})",
                layer::WORKSPACE_WRITE
            ),
            params! {
                "id" => id, "project" => self.scope, "parent" => p.parent_id, "title" => p.title, "brief" => p.brief, "plan" => p.plan,
                "assignee" => p.assignee, "depends" => p.depends_on, "touches" => p.touches, "effort" => p.effort, "now" => now
            },
        )
        .await?;
        self.set_temporal("Task", &id, "pending", false).await?;
        self.index("task_id", "Task", "id").await?;
        let key = params! { "id" => id };
        match &p.parent_id {
            Some(parent) => {
                self.run(
                    &format!("{} MATCH (t:Task {{id: $id}}), (p:Task {{id: $parent}}) MERGE (p)-[e:HAS_SUBTASK]->(t) SET {}", layer::WORKSPACE_WRITE, span("e")),
                    params! { "id" => id, "parent" => parent, "now" => now },
                )
                .await?;
            }
            None => self.link_root(layer::WORKSPACE_WRITE, "MATCH (w:Task {id: $id})", key.clone()).await?,
        }
        for dep in &p.depends_on {
            self.run(
                &format!("{} MATCH (t:Task {{id: $id}}), (d:Task {{id: $dep}}) MERGE (t)-[e:DEPENDS_ON]->(d) SET {}", layer::WORKSPACE_WRITE, span("e")),
                params! { "id" => id, "dep" => dep, "now" => now },
            )
            .await?;
        }
        self.write_contract(&id, &p.contract).await?;
        self.link_code("MATCH (w:Task {id: $id})", &key, "TOUCHES", &p.touches).await?;
        self.task_get(&id).await?.ok_or_else(|| RpcError::internal("task vanished after creation"))
    }

    /// Replace a task's contract checks: the ones in force are retired, the new ones written.
    async fn write_contract(&self, id: &str, contract: &Contract) -> Result<(), RpcError> {
        let now = now_ms();
        self.run(
            &format!(
                "{} MATCH (t:Task {{id: $id}})-[e:HAS_CHECK]->(c:ContractCheck) WHERE c.ended_ms IS NULL SET c.ended_ms = $now, c.ended_reason = 'replaced', c.state = 'gone', c.last_ms = $now, e.ended_ms = $now, e.last_ms = $now",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => id, "now" => now },
        )
        .await?;
        if contract.checks.is_empty() {
            return Ok(());
        }
        let rows: Vec<Value> = contract
            .checks
            .iter()
            .enumerate()
            .map(|(i, c)| json!({"name": c.name, "kind": c.kind, "spec": c.spec, "expect": c.expect, "position": i}))
            .collect();
        self.run(
            &format!(
                "{} MATCH (t:Task {{id: $id}}) UNWIND $rows AS r CREATE (t)-[:HAS_CHECK {{position: r.position, first_ms: $now, last_ms: $now}}]->(c:ContractCheck {{name: r.name, kind: r.kind, spec: r.spec, expect: r.expect, position: r.position, task_id: $id, first_ms: $now, last_ms: $now}})",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => id, "rows" => rows, "now" => now },
        )
        .await?;
        Ok(())
    }

    async fn task_row(&self, id: &str) -> Result<Option<Vec<Value>>, RpcError> {
        let rows = self.run(&format!("{} MATCH (t:Task {{id: $id}}) WHERE t.ended_ms IS NULL RETURN {TASK_COLS}", layer::WORKSPACE_READ), params! { "id" => id }).await?;
        Ok(rows.rows.into_iter().next())
    }

    fn task_from(row: &[Value]) -> Task {
        Task {
            id: s(row, 0),
            project_id: s(row, 1),
            parent_id: opt_s(row, 2),
            title: s(row, 3),
            brief: s(row, 4),
            plan: s(row, 5),
            contract: Contract::default(),
            status: s(row, 6),
            assignee: opt_s(row, 7),
            depends_on: list(row, 8),
            touches: list(row, 9),
            result: opt_s(row, 10),
            session_id: opt_s(row, 11),
            created_ms: n(row, 12),
            updated_ms: n(row, 13),
            contract_hash: opt_s(row, 14),
            // The lineage is `ContractRevision` nodes (filled in by `complete`); an earlier version
            // kept it as JSON text, read here until the task's next revision rewrites it.
            contract_revisions: opt_s(row, 15).and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default(),
            effort: opt_s(row, 16).filter(|effort| !effort.is_empty()),
        }
    }

    /// Contract checks and current touch targets for a set of tasks.
    async fn complete(&self, tasks: &mut [Task]) -> Result<(), RpcError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
        let checks = self
            .run(
                &format!(
                    "{} UNWIND $ids AS i MATCH (t:Task {{id: i}})-[:HAS_CHECK]->(c:ContractCheck) WHERE c.ended_ms IS NULL RETURN t.id, c.position, c.name, c.kind, c.spec, c.expect",
                    layer::WORKSPACE_READ
                ),
                params! { "ids" => ids },
            )
            .await?;
        let mut by_task: HashMap<String, Vec<(u64, ContractCheck)>> = HashMap::new();
        for row in &checks.rows {
            by_task.entry(s(row, 0)).or_default().push((n(row, 1), ContractCheck { name: s(row, 2), kind: s(row, 3), spec: s(row, 4), expect: opt_s(row, 5) }));
        }
        // Touch links follow their code node through edits and renames: report the node's current sid.
        let touches = self
            .run(
                &format!("{} UNWIND $ids AS i MATCH (t:Task {{id: i}})-[r:TOUCHES]->(c:Code) WHERE r.ended_ms IS NULL RETURN t.id, r.ref, r.position, c.sid, c.kind, c.path", layer::ALL_READ),
                params! { "ids" => ids },
            )
            .await?;
        let revisions = self
            .run(&format!("{} UNWIND $ids AS i MATCH (t:Task {{id: i}})-[:REVISED]->(r:ContractRevision) RETURN i, r", layer::WORKSPACE_READ), params! { "ids" => ids })
            .await?;
        let mut lineage: HashMap<String, Vec<(u64, ContractRevision)>> = HashMap::new();
        for row in &revisions.rows {
            let Some(props) = row.get(1).and_then(|node| node.get("properties")).cloned() else { continue };
            let position = props.get("position").and_then(Value::as_u64).unwrap_or(0);
            if let Ok(revision) = serde_json::from_value::<ContractRevision>(props) {
                lineage.entry(s(row, 0)).or_default().push((position, revision));
            }
        }
        let mut touch_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        for row in &touches.rows {
            let reference = s(row, 1);
            let current = if s(row, 4) == "file" { s(row, 5) } else { s(row, 3) };
            touch_map.entry(s(row, 0)).or_default().insert(reference, current);
        }
        for task in tasks.iter_mut() {
            if let Some(mut revisions) = lineage.remove(&task.id) {
                revisions.sort_by_key(|(position, _)| *position);
                task.contract_revisions = revisions.into_iter().map(|(_, revision)| revision).collect();
            }
            if let Some(mut checks) = by_task.remove(&task.id) {
                checks.sort_by_key(|(p, _)| *p);
                task.contract = Contract { checks: checks.into_iter().map(|(_, c)| c).collect() };
            }
            if let Some(map) = touch_map.get(&task.id) {
                for touch in task.touches.iter_mut() {
                    if let Some(current) = map.get(touch) {
                        *touch = current.clone();
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn task_get(&self, id: &str) -> Result<Option<Task>, RpcError> {
        let Some(row) = self.task_row(id).await? else { return Ok(None) };
        let mut tasks = vec![Self::task_from(&row)];
        self.complete(&mut tasks).await?;
        Ok(tasks.pop())
    }

    pub async fn task_update(&self, p: TaskUpdateParams) -> Result<Option<Task>, RpcError> {
        let Some(current) = self.task_get(&p.id).await? else { return Ok(None) };
        // A frozen contract changes only through an accepted revision (the shared rule).
        let change = contract_change(&current, &p, now_ms()).map_err(RpcError::invalid_params)?;
        let now = now_ms();
        let mut sets: Vec<String> = vec!["t.updated_ms = $now".into(), "t.last_ms = $now".into()];
        let mut values = params! { "id" => p.id, "now" => now };
        let mut set = |field: &str, value: Option<Value>| {
            if let Some(v) = value {
                sets.push(format!("t.{field} = ${field}"));
                values.insert(field.to_owned(), v);
            }
        };
        set("title", p.title.clone().map(Value::from));
        set("name", p.title.clone().map(Value::from));
        set("brief", p.brief.map(Value::from));
        set("plan", p.plan.map(Value::from));
        set("assignee", p.assignee.map(Value::from));
        set("result", p.result.map(Value::from));
        set("session_id", p.session_id.map(Value::from));
        set("touches", p.touches.clone().map(|t| json!(t)));
        set("effort", p.effort.map(Value::from));
        set("contract_hash", change.hash.clone().map(Value::from));
        if change.revision.is_some() {
            sets.push("t.contract_revisions = null".into());
        }
        self.run(&format!("{} MATCH (t:Task {{id: $id}}) SET {}", layer::WORKSPACE_WRITE, sets.join(", ")), values).await?;
        if let Some(status) = &p.status {
            self.set_temporal("Task", &p.id, status, false).await?;
        }
        if let Some(revision) = &change.revision {
            // The lineage so far (an earlier version's text included) as nodes, then the new one.
            let mut lineage = current.contract_revisions.clone();
            lineage.push(revision.clone());
            for (position, revision) in lineage.iter().enumerate() {
                let mut props = match serde_json::to_value(revision) {
                    Ok(Value::Object(map)) => map,
                    _ => continue,
                };
                props.insert("id".into(), json!(format!("{}#{position}", p.id)));
                props.insert("task_id".into(), json!(p.id));
                props.insert("position".into(), json!(position));
                props.insert("first_ms".into(), json!(revision.ts_ms));
                props.insert("last_ms".into(), json!(revision.ts_ms));
                self.run(
                    &format!(
                        "{} MATCH (t:Task {{id: $task}}) MERGE (r:ContractRevision {{id: $rid}}) SET r += $props MERGE (t)-[e:REVISED]->(r) SET {}",
                        layer::WORKSPACE_WRITE,
                        span("e")
                    ),
                    params! { "task" => p.id, "rid" => format!("{}#{position}", p.id), "props" => props, "now" => now },
                )
                .await?;
            }
        }
        if let Some(contract) = &change.contract {
            self.write_contract(&p.id, contract).await?;
        }
        if let Some(touches) = &p.touches {
            // The touch links in force end; the ones still named are opened again.
            self.end_edges("MATCH (w:Task {id: $id})", params! { "id" => p.id }, "TOUCHES").await?;
            self.link_code("MATCH (w:Task {id: $id})", &params! { "id" => p.id }, "TOUCHES", touches).await?;
        }
        self.task_get(&p.id).await
    }

    pub async fn task_list(&self, status: Option<&str>, parent: Option<&str>) -> Result<Vec<Task>, RpcError> {
        let rows = self.run(&format!("{} MATCH (t:Task) WHERE t.ended_ms IS NULL RETURN {TASK_COLS}", layer::WORKSPACE_READ), Map::new()).await?;
        let mut tasks: Vec<Task> = rows
            .rows
            .iter()
            .map(|r| Self::task_from(r))
            .filter(|t| status.is_none_or(|s| t.status == s) && parent.is_none_or(|p| t.parent_id.as_deref() == Some(p)))
            .collect();
        tasks.sort_by(|a, b| (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id)));
        self.complete(&mut tasks).await?;
        Ok(tasks)
    }

    // ---- board ---------------------------------------------------------------------------------

    /// The board's stream, with its retention; an earlier version's `BoardMessage` nodes moved
    /// into it (in the order posted, each once) and retired. Once per process and project.
    async fn board_ready(&self) -> Result<(), RpcError> {
        let key = format!("{}|board", self.ig_id);
        if done_get(&key).is_some() {
            return Ok(());
        }
        self.graph.create_topic_kept(&self.ig_id, BOARD_TOPIC, Some(BOARD_RETENTION_DAYS)).await?;
        let rows = self
            .run(&format!("{} MATCH (m:BoardMessage) WHERE m.ended_ms IS NULL RETURN m.id, m.project_id, m.kind, m.author, m.text, m.refs, m.to, m.ts_ms", layer::WORKSPACE_READ), Map::new())
            .await?;
        if !rows.rows.is_empty() {
            let mut old: Vec<BoardMessage> = rows
                .rows
                .iter()
                .map(|r| BoardMessage { id: s(r, 0), project_id: s(r, 1), kind: s(r, 2), author: s(r, 3), text: s(r, 4), refs: list(r, 5), to: opt_s(r, 6), ts_ms: n(r, 7) })
                .collect();
            old.sort_by(|a, b| (a.ts_ms, &a.id).cmp(&(b.ts_ms, &b.id)));
            // A move that stopped part-way appended some already.
            let kept: HashSet<String> = self.board_read(None).await?.into_iter().map(|m| m.id).collect();
            let records = old.iter().filter(|m| !kept.contains(&m.id)).map(board_envelope).collect::<Result<Vec<_>, _>>()?;
            if !records.is_empty() {
                self.graph.stream_append(&self.ig_id, BOARD_TOPIC, records).await?;
            }
            let ids: Vec<String> = old.into_iter().map(|m| m.id).collect();
            for chunk in ids.chunks(CHUNK) {
                self.link_root(layer::WORKSPACE_WRITE, "UNWIND $ids AS i MATCH (w:BoardMessage {id: i})", params! { "ids" => chunk }).await?;
                self.run(
                    &format!(
                        "{} UNWIND $ids AS i MATCH (m:BoardMessage {{id: i}}) WHERE m.ended_ms IS NULL SET m.ended_ms = $now, m.ended_reason = 'moved to the board stream', m.state = 'gone', {}",
                        layer::WORKSPACE_WRITE,
                        span("m")
                    ),
                    params! { "ids" => chunk, "now" => now_ms() },
                )
                .await?;
            }
            tracing::info!(project = %self.scope, messages = ids.len(), "board messages moved into the board stream");
        }
        done_set(key, true);
        Ok(())
    }

    /// The board's messages in the order posted, from the first posted at or after `since_ms`
    /// (found by halving: the stream is in time order).
    async fn board_read(&self, since_ms: Option<u64>) -> Result<Vec<BoardMessage>, RpcError> {
        let (_, _, end) = self.graph.stream_page(&self.ig_id, BOARD_TOPIC, 0, 1, BOARD_PAGE_BYTES).await?;
        let mut offset = 0u64;
        if let Some(since) = since_ms {
            let (mut low, mut high) = (0u64, end);
            while low < high {
                let middle = low + (high - low) / 2;
                let (records, _, _) = self.graph.stream_page(&self.ig_id, BOARD_TOPIC, middle, 1, BOARD_PAGE_BYTES).await?;
                match records.first().map(|(at, time, bytes)| (*at, board_record(bytes).map(|m| m.ts_ms).unwrap_or(*time))) {
                    // Retention removed what came before: the first record kept is the next one.
                    Some((at, ts)) if ts < since => low = at.max(middle) + 1,
                    Some(_) => high = middle,
                    None => low = middle + 1,
                }
            }
            offset = low;
        }
        let mut out = Vec::new();
        while offset < end {
            let (records, next, _) = self.graph.stream_page(&self.ig_id, BOARD_TOPIC, offset, 1000, BOARD_PAGE_BYTES).await?;
            if records.is_empty() || next <= offset {
                break;
            }
            out.extend(records.iter().filter_map(|(_, _, bytes)| board_record(bytes)));
            offset = next;
        }
        Ok(out)
    }

    pub async fn board_post(&self, p: BoardPostParams) -> Result<BoardMessage, RpcError> {
        self.board_ready().await?;
        let message = BoardMessage { id: new_id("msg"), project_id: self.scope.clone(), kind: p.kind, author: p.author, text: p.text, refs: p.refs, to: p.to, ts_ms: now_ms() };
        self.graph.stream_append(&self.ig_id, BOARD_TOPIC, vec![board_envelope(&message)?]).await?;
        Ok(message)
    }

    pub async fn board_list(&self, p: &BoardListParams) -> Result<Vec<BoardMessage>, RpcError> {
        self.board_ready().await?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut messages: Vec<BoardMessage> = self
            .board_read(p.since_ms)
            .await?
            .into_iter()
            .filter(|m| p.since_ms.is_none_or(|since| m.ts_ms >= since))
            // Addressed to the reader, or to everyone.
            .filter(|m| p.to.as_ref().is_none_or(|to| m.to.is_none() || m.to.as_ref() == Some(to)))
            .filter(|m| seen.insert(m.id.clone()))
            .collect();
        // Stable: messages of one millisecond stay in the order posted.
        messages.sort_by_key(|m| m.ts_ms);
        Ok(messages)
    }

    // ---- agents --------------------------------------------------------------------------------

    pub async fn agent_upsert(&self, mut record: AgentRecord) -> Result<AgentRecord, RpcError> {
        record.project_id = self.scope.clone();
        if record.updated_ms == 0 {
            record.updated_ms = now_ms();
        }
        let now = now_ms();
        self.run(
            &format!(
                "{} MERGE (a:Agent {{name: $name}}) SET a.project_id = $project, a.role = $role, a.charter = $charter, a.session_id = $session, a.task_id = $task, a.workspace = $workspace, a.tasks_done = $done, a.tasks_failed = $failed, a.updated_ms = $updated, a.mode = $mode, {}, a.state = null, {}",
                layer::WORKSPACE_WRITE,
                revive("a"),
                span("a")
            ),
            params! {
                "name" => record.name, "project" => record.project_id, "role" => record.role, "charter" => record.charter,
                "session" => record.session_id, "task" => record.task_id, "workspace" => record.workspace, "done" => record.tasks_done,
                "failed" => record.tasks_failed, "updated" => record.updated_ms, "mode" => record.mode.clone(), "now" => now
            },
        )
        .await?;
        self.set_temporal("Agent", &record.name, &record.status, false).await?;
        self.index("agent_name", "Agent", "name").await?;
        self.link_root(layer::WORKSPACE_WRITE, "MATCH (w:Agent {name: $name})", params! { "name" => record.name }).await?;
        // Its task (`task_id`) is its WORKS_ON edge; one to another task ends.
        self.run(
            &format!(
                "{} MATCH (a:Agent {{name: $name}})-[r:WORKS_ON]->(t:Task) WHERE r.ended_ms IS NULL AND ($task IS NULL OR t.id <> $task) SET r.ended_ms = $now, r.last_ms = $now",
                layer::WORKSPACE_WRITE
            ),
            params! { "name" => record.name, "task" => record.task_id, "now" => now },
        )
        .await?;
        if let Some(task) = &record.task_id {
            self.run(
                &format!("{} MATCH (a:Agent {{name: $name}}), (t:Task {{id: $task}}) MERGE (a)-[r:WORKS_ON]->(t) SET r.ended_ms = null, {}", layer::WORKSPACE_WRITE, span("r")),
                params! { "name" => record.name, "task" => task, "now" => now },
            )
            .await?;
        }
        Ok(record)
    }

    pub async fn agent_list(&self) -> Result<Vec<AgentRecord>, RpcError> {
        let rows = self
            .run(
                &format!(
                    "{} MATCH (a:Agent) WHERE a.ended_ms IS NULL RETURN a.name, a.project_id, a.role, a.charter, a.status, a.session_id, a.task_id, a.workspace, a.tasks_done, a.tasks_failed, a.updated_ms, a.mode",
                    layer::WORKSPACE_READ
                ),
                Map::new(),
            )
            .await?;
        let mut agents: Vec<AgentRecord> = rows
            .rows
            .iter()
            .map(|r| AgentRecord {
                name: s(r, 0),
                project_id: s(r, 1),
                role: s(r, 2),
                charter: s(r, 3),
                status: s(r, 4),
                session_id: opt_s(r, 5),
                task_id: opt_s(r, 6),
                workspace: opt_s(r, 7),
                tasks_done: n(r, 8) as u32,
                tasks_failed: n(r, 9) as u32,
                updated_ms: n(r, 10),
                mode: opt_s(r, 11).filter(|mode| !mode.is_empty()).unwrap_or_else(|| "auto".into()),
            })
            .collect();
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(agents)
    }

    // ---- rules ---------------------------------------------------------------------------------

    pub async fn rule_upsert(&self, mut rule: Rule) -> Result<Rule, RpcError> {
        if rule.id.is_empty() {
            rule.id = new_id("rule");
        }
        rule.scope = self.scope.clone();
        rule.updated_ms = now_ms();
        let name: String = rule.text.chars().take(80).collect();
        self.run(
            &format!(
                "{} MERGE (r:Rule {{id: $id}}) SET r.name = $name, r.scope = $scope, r.text = $text, r.source = $source, r.applies_to = $applies, r.updated_ms = $now, {}, r.state = null, {}",
                layer::WORKSPACE_WRITE,
                revive("r"),
                span("r")
            ),
            params! { "id" => rule.id, "name" => name, "scope" => rule.scope, "text" => rule.text, "source" => rule.source, "applies" => rule.applies_to, "now" => rule.updated_ms },
        )
        .await?;
        self.index("rule_id", "Rule", "id").await?;
        self.link_root(layer::WORKSPACE_WRITE, "MATCH (w:Rule {id: $id})", params! { "id" => rule.id }).await?;
        // The code it constrained ends; what it still names is linked again.
        self.end_edges("MATCH (w:Rule {id: $id})", params! { "id" => rule.id }, "CONSTRAINS").await?;
        self.link_code("MATCH (w:Rule {id: $id})", &params! { "id" => rule.id }, "CONSTRAINS", &rule.applies_to).await?;
        Ok(rule)
    }

    pub async fn rule_list(&self) -> Result<Vec<Rule>, RpcError> {
        let rows = self
            .run(&format!("{} MATCH (r:Rule) WHERE r.ended_ms IS NULL RETURN r.id, r.scope, r.text, r.source, r.applies_to, r.updated_ms, r.position", layer::WORKSPACE_READ), Map::new())
            .await?;
        let mut rules: Vec<(u64, Rule)> = rows
            .rows
            .iter()
            .map(|r| (n(r, 6), Rule { id: s(r, 0), scope: s(r, 1), text: s(r, 2), source: s(r, 3), applies_to: list(r, 4), updated_ms: n(r, 5) }))
            .collect();
        rules.sort_by(|a, b| (a.1.source != "cortex_md", a.0, &a.1.id).cmp(&(b.1.source != "cortex_md", b.0, &b.1.id)));
        Ok(rules.into_iter().map(|(_, r)| r).collect())
    }

    /// Mirror a CORTEX.md into `cortex_md` rules: one rule per list item or paragraph, in order.
    /// Rules that are no longer in the file are retired. Returns the number of rules.
    pub async fn import_cortex_md(&self, text: &str) -> Result<usize, RpcError> {
        let items = parse_cortex_md(text);
        let now = now_ms();
        let mut ids = Vec::new();
        for (position, (section, item)) in items.iter().enumerate() {
            let id = format!("md-{}", &crate::facts::hash_hex(format!("{}\u{0}{section}\u{0}{item}", self.scope).as_bytes())[..20]);
            let name: String = item.chars().take(80).collect();
            self.run(
                &format!(
                    "{} MERGE (r:Rule {{id: $id}}) SET r.name = $name, r.scope = $scope, r.text = $text, r.source = 'cortex_md', r.section = $section, r.applies_to = [], r.position = $position, r.updated_ms = $now, {}, r.state = null, {}",
                    layer::WORKSPACE_WRITE,
                    revive("r"),
                    span("r")
                ),
                params! { "id" => id, "name" => name, "scope" => self.scope, "text" => item, "section" => section, "position" => position, "now" => now },
            )
            .await?;
            ids.push(id);
        }
        if !items.is_empty() {
            self.index("rule_id", "Rule", "id").await?;
            for chunk in ids.chunks(CHUNK) {
                self.link_root(layer::WORKSPACE_WRITE, "UNWIND $ids AS i MATCH (w:Rule {id: i})", params! { "ids" => chunk }).await?;
            }
        }
        let existing = self.run(&format!("{} MATCH (r:Rule) WHERE r.source = 'cortex_md' AND r.ended_ms IS NULL RETURN r.id", layer::WORKSPACE_READ), Map::new()).await?;
        let stale: Vec<String> = existing.column(0).filter_map(|v| v.as_str()).filter(|id| !ids.iter().any(|x| x == id)).map(str::to_owned).collect();
        if !stale.is_empty() {
            self.run(
                &format!(
                    "{} UNWIND $ids AS i MATCH (r:Rule {{id: i}}) WHERE r.ended_ms IS NULL SET r.ended_ms = $now, r.ended_reason = 'no longer in CORTEX.md', r.state = 'gone', r.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "ids" => stale, "now" => now },
            )
            .await?;
        }
        Ok(items.len())
    }

    // ---- style ---------------------------------------------------------------------------------

    pub async fn style_upsert(&self, mut entry: StyleEntry) -> Result<StyleEntry, RpcError> {
        if entry.id.is_empty() {
            entry.id = new_id("style");
        }
        entry.scope = self.scope.clone();
        entry.updated_ms = now_ms();
        self.run(
            &format!(
                "{} MERGE (s:Style {{id: $id}}) SET s.name = $area, s.area = $area, s.text = $text, s.scope = $scope, s.confidence = $confidence, s.evidence = $evidence, s.updated_ms = $now, {}",
                layer::WORKSPACE_WRITE,
                span("s")
            ),
            params! { "id" => entry.id, "area" => entry.area, "text" => entry.text, "scope" => entry.scope, "confidence" => entry.confidence, "evidence" => entry.evidence, "now" => entry.updated_ms },
        )
        .await?;
        self.index("style_id", "Style", "id").await?;
        self.link_root(layer::WORKSPACE_WRITE, "MATCH (w:Style {id: $id})", params! { "id" => entry.id }).await?;
        Ok(entry)
    }

    pub async fn style_list(&self) -> Result<Vec<StyleEntry>, RpcError> {
        let rows = self
            .run(&format!("{} MATCH (s:Style) WHERE s.ended_ms IS NULL RETURN s.id, s.area, s.text, s.scope, s.confidence, s.evidence, s.updated_ms", layer::WORKSPACE_READ), Map::new())
            .await?;
        let mut entries: Vec<StyleEntry> = rows
            .rows
            .iter()
            .map(|r| StyleEntry {
                id: s(r, 0),
                area: s(r, 1),
                text: s(r, 2),
                scope: s(r, 3),
                confidence: r.get(4).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                evidence: list(r, 5),
                updated_ms: n(r, 6),
            })
            .collect();
        entries.sort_by(|a, b| (&a.area, &a.id).cmp(&(&b.area, &b.id)));
        Ok(entries)
    }

    // ---- runs ----------------------------------------------------------------------------------

    pub async fn run_record(&self, p: RunRecordParams) -> Result<String, RpcError> {
        let id = new_id("run");
        let now = now_ms();
        self.run(
            &format!(
                "{} CREATE (r:Run {{id: $id, name: $outcome, project_id: $project, session_id: $session, task_id: $task, outcome: $outcome, summary: $summary, touched: $touched, ts_ms: $now, first_ms: $now, last_ms: $now}})",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => id, "project" => self.scope, "session" => p.session_id, "task" => p.task_id, "outcome" => p.outcome, "summary" => p.summary, "touched" => p.touched, "now" => now },
        )
        .await?;
        // The run's measures are its own properties (`turns`, `tool_calls`, …): scalars only.
        let metrics: Map<String, Value> = p
            .metrics
            .as_object()
            .map(|map| {
                map.iter()
                    .filter(|(name, value)| !value.is_object() && !value.is_array() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
                    .map(|(name, value)| (format!("metric_{name}"), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if !metrics.is_empty() {
            self.run(&format!("{} MATCH (r:Run {{id: $id}}) SET r += $metrics", layer::WORKSPACE_WRITE), params! { "id" => id, "metrics" => metrics }).await?;
        }
        self.index("run_id", "Run", "id").await?;
        self.link_root(layer::WORKSPACE_WRITE, "MATCH (w:Run {id: $id})", params! { "id" => id }).await?;
        if let Some(task) = &p.task_id {
            self.run(
                &format!("{} MATCH (r:Run {{id: $id}}), (t:Task {{id: $task}}) CREATE (r)-[:FOR_TASK {{first_ms: $now, last_ms: $now}}]->(t)", layer::WORKSPACE_WRITE),
                params! { "id" => id, "task" => task, "now" => now },
            )
            .await?;
        }
        for reference in &p.touched {
            if let Some(target) = self.code_sid(reference).await? {
                self.run(
                    &format!(
                        "{} MATCH (r:Run {{id: $id}}) MATCH (c:Code {{sid: $target}}) WHERE c.ended_ms IS NULL CREATE (c)-[:MODIFIED_IN {{first_ms: $now, last_ms: $now}}]->(r)",
                        layer::WORKSPACE_WRITE
                    ),
                    params! { "id" => id, "target" => target, "now" => now },
                )
                .await?;
            }
        }
        Ok(id)
    }
}

/// CORTEX.md structure (markdown syntax only): headings name sections; every list item (with its
/// indented continuation lines) and every plain paragraph is one rule. Fenced code is skipped.
pub fn parse_cortex_md(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut section = String::new();
    let mut current: Option<String> = None;
    let mut in_fence = false;
    let flush = |current: &mut Option<String>, out: &mut Vec<(String, String)>, section: &str| {
        if let Some(item) = current.take() {
            let item = item.trim().to_owned();
            if !item.is_empty() {
                out.push((section.to_owned(), item));
            }
        }
    };
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush(&mut current, &mut out, &section);
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            flush(&mut current, &mut out, &section);
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix('#') {
            flush(&mut current, &mut out, &section);
            section = heading.trim_start_matches('#').trim().to_owned();
            continue;
        }
        let bullet = ["- ", "* ", "+ "].iter().find_map(|b| trimmed.strip_prefix(b)).or_else(|| {
            let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() { trimmed[digits.len()..].strip_prefix(". ").or_else(|| trimmed[digits.len()..].strip_prefix(") ")) } else { None }
        });
        // A bullet at the item's own indentation starts a new rule; deeper bullets continue it.
        let indent = line.len() - trimmed.len();
        match bullet {
            Some(body) if indent < 2 || current.is_none() => {
                flush(&mut current, &mut out, &section);
                current = Some(body.trim().to_owned());
            }
            _ => {
                let piece = bullet.unwrap_or(trimmed);
                match &mut current {
                    Some(c) => {
                        c.push(' ');
                        c.push_str(piece.trim());
                    }
                    None => current = Some(piece.trim().to_owned()),
                }
            }
        }
    }
    flush(&mut current, &mut out, &section);
    out
}
