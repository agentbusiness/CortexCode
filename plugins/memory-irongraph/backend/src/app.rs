//! The app's own records in the main project (`db`), WORKSPACE layer (shared with CortexTerm): its
//! projects and sessions as nodes with their fields as properties, linked to each other.
//!
//! ```text
//! (:Project {id, root, name, flows, created_ms, first_ms, last_ms, ended_ms?, state?})
//!     -[:HAS_SESSION]->(:Session {id, project_id, kind, title, agent, parent, workspace, task_id,
//!                                 status, model, flow, topic, created_ms, updated_ms, first_ms, last_ms})
//! (:Session)-[:CHILD_OF]->(:Session)          a session started from another (an agent's run)
//! (:Project)-[:HAS_SETTINGS]->(:Settings)     its settings (see `settings`)
//! stream `app.sessions`                       every change of a session: its fields as written
//! topic `session.<id>`                        journal patches of frames before journals moved into files
//! ```
//!
//! A deleted project is retired, not removed: its sessions and settings go, and its node keeps
//! `ended_ms` and `state: 'gone'`, so a plugin that starts later still learns of the deletion
//! (`memory.app.deleted`). Opening the project again brings it back. Records an earlier version
//! kept as JSON text (`doc`) are rewritten as properties when first read.

use crate::db::{Graph, layer};
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::frame::{JournalPatch, Project, ProjectDeleted, Session};
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Journal patches read per call when the caller names no page size.
const PAGE: usize = 2000;
/// Bytes read per call at most.
const PAGE_BYTES: usize = 32 << 20;
/// The stream of session changes.
pub const SESSIONS_TOPIC: &str = "app.sessions";

const PROJECT_FIELDS: &[&str] = &["id", "root", "name", "created_ms", "flows"];
const SESSION_FIELDS: &[&str] = &["id", "project_id", "kind", "title", "agent", "parent", "workspace", "task_id", "status", "created_ms", "updated_ms", "model", "flow"];

pub struct AppStore {
    graph: Arc<Graph>,
    ig_id: String,
}

/// A session's journal stream.
pub fn topic(session_id: &str) -> String {
    format!("session.{session_id}")
}

/// A record's fields as properties (absent optional fields as null, which removes them).
fn fields<T: serde::Serialize>(record: &T, names: &[&str]) -> Result<Map<String, Value>, RpcError> {
    let Value::Object(map) = serde_json::to_value(record).map_err(|error| RpcError::internal(error.to_string()))? else {
        return Err(RpcError::internal("a record is not an object"));
    };
    Ok(names.iter().map(|name| ((*name).to_owned(), map.get(*name).cloned().unwrap_or(Value::Null))).collect())
}

/// `n.a = $a, n.b = $b, …` for the field names.
fn assignments(var: &str, names: &[&str]) -> String {
    names.iter().map(|name| format!("{var}.{name} = ${name}")).collect::<Vec<_>>().join(", ")
}

impl AppStore {
    pub async fn open(graph: Arc<Graph>, ig_id: String) -> Result<Self, RpcError> {
        let store = Self { graph, ig_id };
        store.graph.ensure_index(&store.ig_id, "app_project_id", "Project", "id").await?;
        store.graph.ensure_index(&store.ig_id, "app_session_id", "Session", "id").await?;
        store.migrate().await?;
        Ok(store)
    }

    async fn run(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    /// Rewrite records an earlier version kept as JSON text (`doc`) as properties.
    async fn migrate(&self) -> Result<(), RpcError> {
        let mut moved = false;
        for node in self.nodes("Project").await? {
            let Some(text) = node.get("doc").and_then(Value::as_str) else { continue };
            match serde_json::from_str::<Project>(text) {
                Ok(project) => self.put_project(&project).await?,
                Err(error) => tracing::warn!(%error, "an old project record cannot be read"),
            }
            moved = true;
        }
        for node in self.nodes("Session").await? {
            let Some(text) = node.get("doc").and_then(Value::as_str) else { continue };
            match serde_json::from_str::<Session>(text) {
                Ok(session) => self.write_session(&session).await?,
                Err(error) => tracing::warn!(%error, "an old session record cannot be read"),
            }
            moved = true;
        }
        if moved {
            tracing::info!("projects and sessions moved from JSON text into properties");
        }
        Ok(())
    }

    /// The properties of every node of `label` (whole nodes, so a property no record has yet
    /// cannot empty the answer).
    async fn nodes(&self, label: &str) -> Result<Vec<Map<String, Value>>, RpcError> {
        let rows = self.run(&format!("{} MATCH (n:{label}) RETURN n", layer::WORKSPACE_READ), Map::new()).await?;
        Ok(rows.column(0).filter_map(|node| node.get("properties").and_then(Value::as_object).cloned()).collect())
    }

    /// Records of `label` that are not retired, decoded from their properties.
    async fn records<T: serde::de::DeserializeOwned>(&self, label: &str, names: &[&str]) -> Result<Vec<T>, RpcError> {
        let mut out = Vec::new();
        for node in self.nodes(label).await? {
            if node.get("ended_ms").is_some_and(|value| !value.is_null()) {
                continue;
            }
            let map: Map<String, Value> = names.iter().filter_map(|name| node.get(*name).filter(|value| !value.is_null()).map(|value| ((*name).to_owned(), value.clone()))).collect();
            match serde_json::from_value(Value::Object(map)) {
                Ok(record) => out.push(record),
                Err(error) => tracing::warn!(label, %error, "skipping an unreadable app record"),
            }
        }
        Ok(out)
    }

    pub async fn projects(&self) -> Result<Vec<Project>, RpcError> {
        self.records("Project", PROJECT_FIELDS).await
    }

    /// Projects deleted (retired), for plugins that start after the deletion.
    pub async fn deleted(&self) -> Result<Vec<ProjectDeleted>, RpcError> {
        Ok(self
            .nodes("Project")
            .await?
            .iter()
            .filter(|node| node.get("ended_ms").is_some_and(|value| !value.is_null()))
            .filter_map(|node| Some(ProjectDeleted { project_id: node.get("id")?.as_str()?.to_owned(), root: node.get("root").and_then(Value::as_str).unwrap_or("").to_owned() }))
            .collect())
    }

    pub async fn put_project(&self, project: &Project) -> Result<(), RpcError> {
        let mut p = fields(project, PROJECT_FIELDS)?;
        p.insert("now".into(), json!(now_ms()));
        self.run(
            &format!(
                "{} MERGE (p:Project {{id: $id}}) ON CREATE SET p.first_ms = $now SET {}, p.last_ms = $now, p.ended_ms = null, p.state = null, p.doc = null",
                layer::WORKSPACE_WRITE,
                assignments("p", PROJECT_FIELDS)
            ),
            p,
        )
        .await?;
        // Its settings node, when it has one, belongs to it.
        self.run(
            &format!("{} MATCH (p:Project {{id: $id}}), (s:Settings {{scope: $id}}) MERGE (p)-[:HAS_SETTINGS]->(s)", layer::WORKSPACE_WRITE),
            params! { "id" => project.id },
        )
        .await?;
        Ok(())
    }

    /// Retire the project and delete its settings (its sessions are deleted first, by the caller).
    pub async fn delete_project(&self, id: &str) -> Result<(), RpcError> {
        self.run(&format!("{} MATCH (n:Setting {{scope: $id}}) DETACH DELETE n", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        self.run(&format!("{} MATCH (n:SettingSection {{scope: $id}}) DETACH DELETE n", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        self.run(&format!("{} MATCH (s:Settings {{scope: $id}}) DETACH DELETE s", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        self.run(
            &format!("{} MATCH (p:Project {{id: $id}}) SET p.ended_ms = $now, p.state = 'gone', p.last_ms = $now", layer::WORKSPACE_WRITE),
            params! { "id" => id, "now" => now_ms() },
        )
        .await?;
        Ok(())
    }

    pub async fn sessions(&self) -> Result<Vec<Session>, RpcError> {
        self.records("Session", SESSION_FIELDS).await
    }

    async fn write_session(&self, session: &Session) -> Result<(), RpcError> {
        let mut p = fields(session, SESSION_FIELDS)?;
        p.insert("now".into(), json!(now_ms()));
        p.insert("topic".into(), json!(topic(&session.id)));
        self.run(
            &format!(
                "{} MERGE (s:Session {{id: $id}}) ON CREATE SET s.first_ms = $now SET {}, s.topic = $topic, s.last_ms = $now, s.doc = null",
                layer::WORKSPACE_WRITE,
                assignments("s", SESSION_FIELDS)
            ),
            p,
        )
        .await?;
        self.run(
            &format!("{} MATCH (p:Project {{id: $project}}), (s:Session {{id: $id}}) MERGE (p)-[:HAS_SESSION]->(s)", layer::WORKSPACE_WRITE),
            params! { "id" => session.id, "project" => session.project_id },
        )
        .await?;
        if let Some(parent) = &session.parent {
            self.run(
                &format!("{} MATCH (s:Session {{id: $id}}), (parent:Session {{id: $parent}}) MERGE (s)-[:CHILD_OF]->(parent)", layer::WORKSPACE_WRITE),
                params! { "id" => session.id, "parent" => parent },
            )
            .await?;
        }
        Ok(())
    }

    pub async fn put_session(&self, session: &Session) -> Result<(), RpcError> {
        self.write_session(session).await?;
        let record = json!({ "at_ms": now_ms(), "value": fields(session, SESSION_FIELDS)? });
        if let Ok(bytes) = serde_json::to_vec(&record)
            && let Err(error) = self.graph.stream_append(&self.ig_id, SESSIONS_TOPIC, vec![bytes]).await
        {
            tracing::warn!(error = %error.message, "a session change was not added to its stream");
        }
        Ok(())
    }

    /// Session records and their journals.
    pub async fn delete_sessions(&self, ids: &[String]) -> Result<(), RpcError> {
        for id in ids {
            self.graph.drop_topic(&self.ig_id, &topic(id)).await?;
        }
        self.run(&format!("{} UNWIND $ids AS id MATCH (s:Session {{id: id}}) DETACH DELETE s", layer::WORKSPACE_WRITE), params! { "ids" => ids }).await?;
        Ok(())
    }

    /// Append patches to a session's journal, in order. Returns the offset after the last one.
    pub async fn journal_append(&self, session_id: &str, patches: &[JournalPatch]) -> Result<u64, RpcError> {
        let records = patches.iter().map(|patch| serde_json::to_vec(patch).map_err(|error| RpcError::internal(error.to_string()))).collect::<Result<Vec<_>, _>>()?;
        self.graph.stream_append(&self.ig_id, &topic(session_id), records).await
    }

    /// Drop a session's journal stream: the frame moved it into its own file.
    pub async fn journal_drop(&self, session_id: &str) -> Result<(), RpcError> {
        self.graph.drop_topic(&self.ig_id, &topic(session_id)).await
    }

    /// A session's journal patches from `from`: the patches, the offset after them, and whether
    /// nothing follows.
    pub async fn journal_read(&self, session_id: &str, from: u64, max: Option<usize>) -> Result<(Vec<JournalPatch>, u64, bool), RpcError> {
        let (records, next, end) = self.graph.stream_fetch(&self.ig_id, &topic(session_id), from, max.unwrap_or(PAGE).max(1), PAGE_BYTES).await?;
        let mut patches = Vec::with_capacity(records.len());
        for (offset, bytes) in records {
            match serde_json::from_slice::<JournalPatch>(&bytes) {
                Ok(patch) => patches.push(patch),
                Err(error) => tracing::warn!(session_id, offset, %error, "skipping an unreadable journal record"),
            }
        }
        Ok((patches, next, next >= end))
    }
}
