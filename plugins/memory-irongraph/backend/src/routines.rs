//! Routines in the WORKSPACE layer (docs/SCHEDULES-PLAN.md): the scheduler plugin's recurring and
//! timed work, kept in its scope's store (a project's, or the global one).
//!
//! ```text
//! (:Routine {id, scope, state, next_ms, title, kind, doc})
//! (:Routine)-[:WATCHES]->(:Task)          the tasks a tethered routine exists for
//! (:Routine)-[:FIRED]->(:RoutineRun {id, routine_id, due_ms, started_ms, outcome, doc})
//! ```
//!
//! `doc` holds the whole record (JSON text), so a field the scheduler adds needs no change here;
//! the other properties are what the graph is queried and linked by. Sessions live in the app
//! store (another IronGraph project), so session ids stay properties.

use crate::db::layer;
use crate::params;
use crate::work::WorkStore;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{Routine, RoutineRun};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

fn doc<T: serde::Serialize>(record: &T) -> Result<String, RpcError> {
    serde_json::to_string(record).map_err(|error| RpcError::internal(error.to_string()))
}

fn parse<T: DeserializeOwned>(row: &[Value]) -> Option<T> {
    row.first().and_then(Value::as_str).and_then(|text| serde_json::from_str(text).ok())
}

impl WorkStore {
    async fn routine_run_query(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    pub async fn routine_put(&self, mut routine: Routine) -> Result<Routine, RpcError> {
        routine.scope = self.scope.clone();
        self.graph.ensure_index(&self.ig_id, "routine_id", "Routine", "id").await?;
        self.routine_run_query(
            &format!(
                "{} MERGE (r:Routine {{id: $id}}) SET r.name = $title, r.scope = $scope, r.state = $state, r.next_ms = $next, r.title = $title, r.kind = $kind, r.doc = $doc",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => routine.id, "scope" => routine.scope, "state" => routine.state, "next" => routine.next_ms, "title" => routine.title, "kind" => routine.kind, "doc" => doc(&routine)? },
        )
        .await?;
        self.routine_run_query(&format!("{} MATCH (r:Routine {{id: $id}})-[w:WATCHES]->() DELETE w", layer::WORKSPACE_WRITE), params! { "id" => routine.id }).await?;
        for task in &routine.watches {
            self.routine_run_query(
                &format!("{} MATCH (r:Routine {{id: $id}}), (t:Task {{id: $task}}) MERGE (r)-[:WATCHES]->(t)", layer::WORKSPACE_WRITE),
                params! { "id" => routine.id, "task" => task },
            )
            .await?;
        }
        Ok(routine)
    }

    pub async fn routine_get(&self, id: &str) -> Result<Option<Routine>, RpcError> {
        let rows = self.routine_run_query(&format!("{} MATCH (r:Routine {{id: $id}}) RETURN r.doc", layer::WORKSPACE_READ), params! { "id" => id }).await?;
        Ok(rows.rows.first().and_then(|row| parse(row)))
    }

    pub async fn routine_list(&self) -> Result<Vec<Routine>, RpcError> {
        let rows = self.routine_run_query(&format!("{} MATCH (r:Routine) RETURN r.doc", layer::WORKSPACE_READ), Map::new()).await?;
        let mut routines: Vec<Routine> = rows.rows.iter().filter_map(|row| parse(row)).collect();
        routines.sort_by_key(|routine| routine.created_ms);
        Ok(routines)
    }

    pub async fn routine_remove(&self, id: &str) -> Result<bool, RpcError> {
        if self.routine_get(id).await?.is_none() {
            return Ok(false);
        }
        self.routine_run_query(&format!("{} MATCH (x:RoutineRun {{routine_id: $id}}) DETACH DELETE x", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        self.routine_run_query(&format!("{} MATCH (r:Routine {{id: $id}}) DETACH DELETE r", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        Ok(true)
    }

    pub async fn routine_run_put(&self, run: RoutineRun) -> Result<RoutineRun, RpcError> {
        self.graph.ensure_index(&self.ig_id, "routine_run_id", "RoutineRun", "id").await?;
        self.routine_run_query(
            &format!(
                "{} MERGE (x:RoutineRun {{id: $id}}) SET x.name = $outcome, x.routine_id = $routine, x.due_ms = $due, x.started_ms = $started, x.outcome = $outcome, x.doc = $doc",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => run.id, "routine" => run.routine_id, "due" => run.due_ms, "started" => run.started_ms, "outcome" => run.outcome, "doc" => doc(&run)? },
        )
        .await?;
        self.routine_run_query(
            &format!("{} MATCH (r:Routine {{id: $routine}}), (x:RoutineRun {{id: $id}}) MERGE (r)-[:FIRED]->(x)", layer::WORKSPACE_WRITE),
            params! { "id" => run.id, "routine" => run.routine_id },
        )
        .await?;
        Ok(run)
    }

    pub async fn routine_runs(&self, id: &str, limit: usize) -> Result<Vec<RoutineRun>, RpcError> {
        let rows = self
            .routine_run_query(&format!("{} MATCH (x:RoutineRun {{routine_id: $id}}) RETURN x.doc", layer::WORKSPACE_READ), params! { "id" => id })
            .await?;
        let mut runs: Vec<RoutineRun> = rows.rows.iter().filter_map(|row| parse(row)).collect();
        runs.sort_by(|a, b| b.started_ms.cmp(&a.started_ms));
        runs.truncate(limit);
        Ok(runs)
    }
}
