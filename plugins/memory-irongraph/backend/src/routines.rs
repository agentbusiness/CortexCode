//! Routines in the WORKSPACE layer (docs/SCHEDULES-PLAN.md): the scheduler plugin's recurring and
//! timed work, kept in its scope's store (a project's, or the global one).
//!
//! ```text
//! (:Routine {id, scope, title, kind, brief, command, when, schedule_kind, every_ms, days, minutes,
//!            at_ms, cron, next_ms, last_due_ms, state, state_reason, report_to, home_session,
//!            created_session, created_run, manage, until_ms, max_runs, run_count, fail_streak,
//!            last_outcome, last_summary, last_ok_ms, mode, flow, by, created_ms, updated_ms,
//!            provider, provider_schedule})
//! (:Routine)-[:IN]->(root)                the scope's root (see `work`)
//! (:Routine)-[:WATCHES]->(:Task)          the tasks a tethered routine exists for
//! (:Routine)-[:FIRED]->(:RoutineRun {id, routine_id, due_ms, started_ms, ended_ms, outcome,
//!                                    summary, session_id, task_id, provider_run})
//! ```
//!
//! The schedule is the routine's own properties (`schedule_kind`: `interval` | `calendar` |
//! `once` | `cron`, with the fields of its kind). `state` is temporal (its history is kept).
//! Sessions live in the app store (another IronGraph project), so session ids stay properties.
//! Records an earlier version kept as JSON text (`doc`) are rewritten as properties when the
//! scope's routines are first listed. Every node and edge keeps `first_ms` and `last_ms`.
//!
//! Nothing is deleted: a removed routine is retired (`ended_ms`, `ended_reason`, `state:
//! 'gone'`), and its runs stay as its history, marked `state: 'gone'` (a run's own `ended_ms` is
//! when it finished). A watch that no longer holds gets `ended_ms` on its edge. Reads skip them.

use crate::db::layer;
use crate::params;
use crate::work::{WorkStore, revive, span};
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{Routine, RoutineRun};
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value, json};

const ROUTINE_FIELDS: &[&str] = &[
    "id", "scope", "title", "kind", "brief", "command", "when", "next_ms", "last_due_ms", "state", "state_reason", "report_to", "home_session", "created_session",
    "created_run", "manage", "until_ms", "max_runs", "run_count", "fail_streak", "last_outcome", "last_summary", "last_ok_ms", "mode", "flow", "by", "created_ms",
    "updated_ms", "provider", "provider_schedule",
];
const SCHEDULE_FIELDS: &[&str] = &["schedule_kind", "every_ms", "days", "minutes", "at_ms", "cron"];
const RUN_FIELDS: &[&str] = &["id", "routine_id", "due_ms", "started_ms", "ended_ms", "outcome", "summary", "session_id", "task_id", "provider_run"];

/// A record's fields as properties (absent ones as null, which removes them).
fn fields<T: serde::Serialize>(record: &T, names: &[&str]) -> Result<Map<String, Value>, RpcError> {
    let Value::Object(map) = serde_json::to_value(record).map_err(|error| RpcError::internal(error.to_string()))? else {
        return Err(RpcError::internal("a record is not an object"));
    };
    Ok(names.iter().map(|name| ((*name).to_owned(), map.get(*name).cloned().unwrap_or(Value::Null))).collect())
}

/// The schedule (`{kind, …}`) as the routine's own properties.
fn schedule_props(schedule: &Value) -> Map<String, Value> {
    let mut props: Map<String, Value> = SCHEDULE_FIELDS.iter().map(|name| ((*name).to_owned(), Value::Null)).collect();
    if let Value::Object(map) = schedule {
        props.insert("schedule_kind".into(), map.get("kind").cloned().unwrap_or(Value::Null));
        for name in ["every_ms", "days", "minutes", "at_ms"] {
            props.insert(name.into(), map.get(name).cloned().unwrap_or(Value::Null));
        }
        props.insert("cron".into(), map.get("expr").cloned().unwrap_or(Value::Null));
    }
    props
}

/// The schedule rebuilt from the routine's properties.
fn schedule_of(node: &Map<String, Value>) -> Value {
    let Some(kind) = node.get("schedule_kind").and_then(Value::as_str) else { return Value::Null };
    let mut out = Map::new();
    out.insert("kind".into(), json!(kind));
    for name in ["every_ms", "days", "minutes", "at_ms"] {
        if let Some(value) = node.get(name).filter(|value| !value.is_null()) {
            out.insert(name.into(), value.clone());
        }
    }
    if let Some(expr) = node.get("cron").filter(|value| !value.is_null()) {
        out.insert("expr".into(), expr.clone());
    }
    Value::Object(out)
}

fn assignments(var: &str, names: impl Iterator<Item = String>) -> String {
    names.map(|name| format!("{var}.{name} = ${name}")).collect::<Vec<_>>().join(", ")
}

fn decode<T: serde::de::DeserializeOwned>(node: &Map<String, Value>, names: &[&str], extra: Option<(&str, Value)>) -> Option<T> {
    let mut map: Map<String, Value> = names.iter().filter_map(|name| node.get(*name).filter(|value| !value.is_null()).map(|value| ((*name).to_owned(), value.clone()))).collect();
    if let Some((name, value)) = extra {
        map.insert(name.to_owned(), value);
    }
    match serde_json::from_value(Value::Object(map)) {
        Ok(record) => Some(record),
        Err(error) => {
            tracing::warn!(%error, "skipping an unreadable routine record");
            None
        }
    }
}

impl WorkStore {
    async fn routine_run_query(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    /// The properties of the nodes a `MATCH … RETURN n, n.state` finds, with the temporal `state`
    /// read as its own column (its current value).
    async fn routine_nodes(&self, cypher: &str, params: Map<String, Value>) -> Result<Vec<Map<String, Value>>, RpcError> {
        let rows = self.routine_run_query(cypher, params).await?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| {
                let mut props = row.first()?.get("properties").and_then(Value::as_object).cloned()?;
                if let Some(state) = row.get(1).filter(|state| !state.is_null()) {
                    props.insert("state".into(), state.clone());
                }
                Some(props)
            })
            .collect())
    }

    /// Write a routine's node (brought back when it was retired) and its temporal `state`.
    async fn routine_write(&self, routine: &Routine) -> Result<(), RpcError> {
        let mut props = fields(routine, ROUTINE_FIELDS)?;
        props.extend(schedule_props(&routine.schedule));
        let set = assignments("r", props.keys().filter(|name| *name != "id" && *name != "state").cloned());
        props.remove("state");
        props.insert("name".into(), json!(routine.title));
        let now = now_ms();
        props.insert("now".into(), json!(now));
        self.routine_run_query(&format!("{} MERGE (r:Routine {{id: $id}}) SET {set}, r.name = $name, r.doc = null, {}, {}", layer::WORKSPACE_WRITE, revive("r"), span("r")), props).await?;
        // A routine written again after it was removed has its runs back as its history.
        self.routine_run_query(
            &format!(
                "{} MATCH (x:RoutineRun {{routine_id: $id}}) WHERE x.ended_reason = 'routine removed' SET x.state = null, x.ended_reason = null, x.last_ms = $now",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => routine.id, "now" => now },
        )
        .await?;
        self.set_temporal("Routine", &routine.id, &routine.state, false).await?;
        self.link_root(layer::WORKSPACE_WRITE, "MATCH (w:Routine {id: $id})", params! { "id" => routine.id }).await
    }

    pub async fn routine_put(&self, mut routine: Routine) -> Result<Routine, RpcError> {
        routine.scope = self.scope.clone();
        self.graph.ensure_index(&self.ig_id, "routine_id", "Routine", "id").await?;
        self.routine_write(&routine).await?;
        let now = now_ms();
        // Watches it no longer names end; the ones it names are opened (again).
        self.routine_run_query(
            &format!(
                "{} MATCH (r:Routine {{id: $id}})-[w:WATCHES]->(t:Task) WHERE w.ended_ms IS NULL AND NOT t.id IN $tasks SET w.ended_ms = $now, w.last_ms = $now",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => routine.id, "tasks" => routine.watches, "now" => now },
        )
        .await?;
        for task in &routine.watches {
            self.routine_run_query(
                &format!("{} MATCH (r:Routine {{id: $id}}), (t:Task {{id: $task}}) MERGE (r)-[w:WATCHES]->(t) SET w.ended_ms = null, {}", layer::WORKSPACE_WRITE, span("w")),
                params! { "id" => routine.id, "task" => task, "now" => now },
            )
            .await?;
        }
        Ok(routine)
    }

    /// A routine from its node and the tasks it watches; an earlier version's JSON text is
    /// rewritten as properties.
    async fn routine_from(&self, node: &Map<String, Value>) -> Result<Option<Routine>, RpcError> {
        if let Some(text) = node.get("doc").and_then(Value::as_str) {
            let Ok(routine) = serde_json::from_str::<Routine>(text) else { return Ok(None) };
            self.routine_write(&routine).await?;
            return Ok(Some(routine));
        }
        let Some(mut routine) = decode::<Routine>(node, ROUTINE_FIELDS, Some(("schedule", schedule_of(node)))) else { return Ok(None) };
        let rows = self
            .routine_run_query(&format!("{} MATCH (r:Routine {{id: $id}})-[w:WATCHES]->(t:Task) WHERE w.ended_ms IS NULL RETURN t.id", layer::WORKSPACE_READ), params! { "id" => routine.id })
            .await?;
        routine.watches = rows.column(0).filter_map(|id| id.as_str().map(str::to_owned)).collect();
        Ok(Some(routine))
    }

    pub async fn routine_get(&self, id: &str) -> Result<Option<Routine>, RpcError> {
        let nodes = self.routine_nodes(&format!("{} MATCH (r:Routine {{id: $id}}) WHERE r.ended_ms IS NULL RETURN r, r.state", layer::WORKSPACE_READ), params! { "id" => id }).await?;
        match nodes.first() {
            Some(node) => self.routine_from(node).await,
            None => Ok(None),
        }
    }

    pub async fn routine_list(&self) -> Result<Vec<Routine>, RpcError> {
        let nodes = self.routine_nodes(&format!("{} MATCH (r:Routine) WHERE r.ended_ms IS NULL RETURN r, r.state", layer::WORKSPACE_READ), Map::new()).await?;
        let mut routines = Vec::new();
        for node in &nodes {
            if let Some(routine) = self.routine_from(node).await? {
                routines.push(routine);
            }
        }
        routines.sort_by_key(|routine| routine.created_ms);
        Ok(routines)
    }

    pub async fn routine_remove(&self, id: &str) -> Result<bool, RpcError> {
        if self.routine_get(id).await?.is_none() {
            return Ok(false);
        }
        let now = now_ms();
        // Its runs stay as its history (a run's `ended_ms` is when it finished).
        self.routine_run_query(
            &format!("{} MATCH (x:RoutineRun {{routine_id: $id}}) SET x.state = 'gone', x.ended_reason = 'routine removed', x.last_ms = $now", layer::WORKSPACE_WRITE),
            params! { "id" => id, "now" => now },
        )
        .await?;
        self.routine_run_query(
            &format!("{} MATCH (r:Routine {{id: $id}}) WHERE r.ended_ms IS NULL SET r.ended_ms = $now, r.ended_reason = 'removed', r.last_ms = $now", layer::WORKSPACE_WRITE),
            params! { "id" => id, "now" => now },
        )
        .await?;
        self.set_temporal("Routine", id, "gone", false).await?;
        Ok(true)
    }

    pub async fn routine_run_put(&self, run: RoutineRun) -> Result<RoutineRun, RpcError> {
        self.graph.ensure_index(&self.ig_id, "routine_run_id", "RoutineRun", "id").await?;
        let mut props = fields(&run, RUN_FIELDS)?;
        let set = assignments("x", props.keys().filter(|name| *name != "id").cloned());
        let now = now_ms();
        props.insert("name".into(), json!(run.outcome));
        props.insert("now".into(), json!(now));
        self.routine_run_query(&format!("{} MERGE (x:RoutineRun {{id: $id}}) SET {set}, x.name = $name, x.doc = null, {}", layer::WORKSPACE_WRITE, span("x")), props).await?;
        self.routine_run_query(
            &format!("{} MATCH (r:Routine {{id: $routine}}), (x:RoutineRun {{id: $id}}) MERGE (r)-[e:FIRED]->(x) SET {}", layer::WORKSPACE_WRITE, span("e")),
            params! { "id" => run.id, "routine" => run.routine_id, "now" => now },
        )
        .await?;
        Ok(run)
    }

    pub async fn routine_runs(&self, id: &str, limit: usize) -> Result<Vec<RoutineRun>, RpcError> {
        // A removed routine's runs are its history only.
        let nodes = self
            .routine_nodes(&format!("{} MATCH (x:RoutineRun {{routine_id: $id}}) WHERE x.state IS NULL OR x.state <> 'gone' RETURN x", layer::WORKSPACE_READ), params! { "id" => id })
            .await?;
        let mut runs = Vec::new();
        for node in &nodes {
            if let Some(text) = node.get("doc").and_then(Value::as_str) {
                if let Ok(run) = serde_json::from_str::<RoutineRun>(text) {
                    runs.push(self.routine_run_put(run).await?);
                }
                continue;
            }
            if let Some(run) = decode::<RoutineRun>(node, RUN_FIELDS, None) {
                runs.push(run);
            }
        }
        runs.sort_by(|a, b| b.started_ms.cmp(&a.started_ms));
        runs.truncate(limit);
        Ok(runs)
    }
}
