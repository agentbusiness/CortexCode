//! The app's own records in the main project (`db`), WORKSPACE layer (shared with CortexTerm): its
//! projects and sessions as nodes with their fields as properties, linked to each other.
//!
//! ```text
//! (:Project {id, root, name, flows, created_ms, first_ms, last_ms, ended_ms?, state?})-[:ON]->(:Machine)
//!     -[:HAS_SESSION]->(:Session {id, project_id, kind, title, agent, parent, workspace, task_id,
//!                                 status, model, flow, topic, created_ms, updated_ms, first_ms,
//!                                 last_ms, ended_ms?, ended_reason?, state?})
//! (:Session)-[:CHILD_OF]->(:Session)          a session started from another (an agent's run)
//! (:Session)-[:IN]->(:Workspace {key})        its workspace, when that node exists (never made here)
//! (:Project)-[:HAS_SETTINGS]->(:Settings)     its settings (see `settings`)
//! stream `app.sessions`                       every change of a session: {at_ms, value: its fields
//!                                             as written}, or {at_ms, value: {id, project_id, deleted: true}}
//! topic `session.<id>`                        journal patches of frames before journals moved into files
//! ```
//!
//! Every node and edge keeps `first_ms` and `last_ms`. `task_id` stays a property: tasks live in
//! another IronGraph project. Nothing is deleted, only retired (`ended_ms`, `ended_reason`,
//! `state: 'gone'`), with its edges kept, so what other plugins linked to it stays linked. A
//! deleted session is retired and its old journal topic (bytes) dropped; a session written again
//! with the same id is back. A deleted project is retired with its settings (its sessions are
//! retired first, by the caller), so a plugin that starts later still learns of the deletion
//! (`memory.app.deleted`); opening the project again brings it back. Records an earlier version
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
        let now = now_ms();
        let mut p = fields(project, PROJECT_FIELDS)?;
        p.insert("now".into(), json!(now));
        self.run(
            &format!(
                "{} MERGE (p:Project {{id: $id}}) ON CREATE SET p.first_ms = $now SET {}, p.last_ms = $now, p.ended_ms = null, p.ended_reason = null, p.state = null, p.doc = null",
                layer::WORKSPACE_WRITE,
                assignments("p", PROJECT_FIELDS)
            ),
            p,
        )
        .await?;
        // It hangs from this machine, the graph's root.
        self.run(
            &format!("{} MATCH (p:Project {{id: $id}}), (m:Machine) MERGE (p)-[r:ON]->(m) ON CREATE SET r.first_ms = $now SET r.last_ms = $now", layer::WORKSPACE_WRITE),
            params! { "id" => project.id, "now" => now },
        )
        .await?;
        // Its settings node, when it has one, belongs to it.
        self.run(
            &format!(
                "{} MATCH (p:Project {{id: $id}}), (s:Settings {{scope: $id}}) MERGE (p)-[r:HAS_SETTINGS]->(s) ON CREATE SET r.first_ms = $now SET r.last_ms = $now",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => project.id, "now" => now },
        )
        .await?;
        Ok(())
    }

    /// Retire the project and its settings, with their edges kept (its sessions are retired
    /// first, by the caller).
    pub async fn delete_project(&self, id: &str) -> Result<(), RpcError> {
        let now = now_ms();
        for label in ["Setting", "SettingSection", "Settings"] {
            self.run(
                &format!(
                    "{} MATCH (n:{label} {{scope: $id}}) WHERE n.ended_ms IS NULL SET n.ended_ms = $now, n.ended_reason = 'deleted', n.state = 'gone', n.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "id" => id, "now" => now },
            )
            .await?;
        }
        self.run(
            &format!("{} MATCH (p:Project {{id: $id}}) SET p.ended_ms = $now, p.ended_reason = 'deleted', p.state = 'gone', p.last_ms = $now", layer::WORKSPACE_WRITE),
            params! { "id" => id, "now" => now },
        )
        .await?;
        Ok(())
    }

    pub async fn sessions(&self) -> Result<Vec<Session>, RpcError> {
        self.records("Session", SESSION_FIELDS).await
    }

    /// Write a session (back when it was retired) and its edges.
    async fn write_session(&self, session: &Session) -> Result<(), RpcError> {
        let now = now_ms();
        let mut p = fields(session, SESSION_FIELDS)?;
        p.insert("now".into(), json!(now));
        p.insert("topic".into(), json!(topic(&session.id)));
        self.run(
            &format!(
                "{} MERGE (s:Session {{id: $id}}) ON CREATE SET s.first_ms = $now SET {}, s.topic = $topic, s.last_ms = $now, s.ended_ms = null, s.ended_reason = null, s.state = null, s.doc = null",
                layer::WORKSPACE_WRITE,
                assignments("s", SESSION_FIELDS)
            ),
            p,
        )
        .await?;
        self.run(
            &format!(
                "{} MATCH (p:Project {{id: $project}}), (s:Session {{id: $id}}) MERGE (p)-[r:HAS_SESSION]->(s) ON CREATE SET r.first_ms = $now SET r.last_ms = $now",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => session.id, "project" => session.project_id, "now" => now },
        )
        .await?;
        if let Some(parent) = &session.parent {
            self.run(
                &format!(
                    "{} MATCH (s:Session {{id: $id}}), (parent:Session {{id: $parent}}) MERGE (s)-[r:CHILD_OF]->(parent) ON CREATE SET r.first_ms = $now SET r.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "id" => session.id, "parent" => parent, "now" => now },
            )
            .await?;
        }
        // Its workspace, when another plugin has put that node (it is never made here).
        if !session.workspace.is_empty() {
            self.run(
                &format!(
                    "{} MATCH (s:Session {{id: $id}}), (w:Workspace {{key: $workspace}}) MERGE (s)-[r:IN]->(w) ON CREATE SET r.first_ms = $now SET r.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "id" => session.id, "workspace" => session.workspace, "now" => now },
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

    /// Retire sessions, with their edges kept (what other plugins linked to them stays linked),
    /// drop their old journal topics and add each deletion to the `app.sessions` stream.
    pub async fn delete_sessions(&self, ids: &[String]) -> Result<(), RpcError> {
        for id in ids {
            self.graph.drop_topic(&self.ig_id, &topic(id)).await?;
        }
        let now = now_ms();
        let rows = self
            .run(
                &format!(
                    "{} UNWIND $ids AS id MATCH (s:Session {{id: id}}) WHERE s.ended_ms IS NULL SET s.ended_ms = $now, s.ended_reason = 'deleted', s.state = 'gone', s.last_ms = $now RETURN s.id, s.project_id",
                    layer::WORKSPACE_WRITE
                ),
                params! { "ids" => ids, "now" => now },
            )
            .await?;
        let records: Vec<Vec<u8>> = rows
            .rows
            .iter()
            .filter_map(|row| {
                let id = row.first().and_then(Value::as_str)?;
                let project = row.get(1).and_then(Value::as_str).unwrap_or("");
                serde_json::to_vec(&json!({ "at_ms": now, "value": { "id": id, "project_id": project, "deleted": true } })).ok()
            })
            .collect();
        if !records.is_empty()
            && let Err(error) = self.graph.stream_append(&self.ig_id, SESSIONS_TOPIC, records).await
        {
            tracing::warn!(error = %error.message, "session deletions were not added to their stream");
        }
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
