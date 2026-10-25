//! The app's own records in the main project (`db`), WORKSPACE layer: its projects and sessions as
//! nodes, each session's journal as a stream.
//!
//! ```text
//! (:Project {id, root, name, created_ms, doc})-[:HAS_SESSION]->(:Session {id, project_id, topic, doc})
//! (:Session)-[:CHILD_OF]->(:Session)          a session started from another (an agent's run)
//! topic `session.<id>`                          the session's journal patches, one record each
//! ```
//!
//! `doc` holds the record as the frame keeps it (JSON text), so a field the frame adds needs no
//! change here; the other properties are what the graph is queried and linked by. A journal is a
//! stream, not nodes: it is only ever appended to and read in order from an offset.

use crate::db::{Graph, layer};
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::frame::{JournalPatch, Project, Session};
use serde_json::Map;
use std::sync::Arc;

/// Journal patches read per call when the caller names no page size.
const PAGE: usize = 2000;
/// Bytes read per call at most.
const PAGE_BYTES: usize = 32 << 20;

pub struct AppStore {
    graph: Arc<Graph>,
    ig_id: String,
}

/// A session's journal stream.
pub fn topic(session_id: &str) -> String {
    format!("session.{session_id}")
}

fn doc<T: serde::Serialize>(record: &T) -> Result<String, RpcError> {
    serde_json::to_string(record).map_err(|error| RpcError::internal(error.to_string()))
}

impl AppStore {
    pub async fn open(graph: Arc<Graph>, ig_id: String) -> Result<Self, RpcError> {
        Ok(Self { graph, ig_id })
    }

    async fn run(&self, cypher: &str, params: Map<String, serde_json::Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    /// Every record of `label`, decoded from its `doc`.
    async fn docs<T: serde::de::DeserializeOwned>(&self, label: &str) -> Result<Vec<T>, RpcError> {
        let rows = self.run(&format!("{} MATCH (n:{label}) RETURN n.doc", layer::WORKSPACE_READ), Map::new()).await?;
        let mut out = Vec::new();
        for text in rows.column(0).filter_map(|value| value.as_str()) {
            match serde_json::from_str(text) {
                Ok(record) => out.push(record),
                Err(error) => tracing::warn!(label, %error, "skipping an unreadable app record"),
            }
        }
        Ok(out)
    }

    pub async fn projects(&self) -> Result<Vec<Project>, RpcError> {
        self.docs("Project").await
    }

    pub async fn put_project(&self, project: &Project) -> Result<(), RpcError> {
        self.run(
            &format!("{} MERGE (p:Project {{id: $id}}) SET p.root = $root, p.name = $name, p.created_ms = $created, p.doc = $doc", layer::WORKSPACE_WRITE),
            params! { "id" => project.id, "root" => project.root, "name" => project.name, "created" => project.created_ms, "doc" => doc(project)? },
        )
        .await?;
        self.graph.ensure_index(&self.ig_id, "app_project_id", "Project", "id").await?;
        // Its settings node, when it has one, belongs to it.
        self.run(
            &format!("{} MATCH (p:Project {{id: $id}}), (s:Settings {{scope: $id}}) MERGE (p)-[:HAS_SETTINGS]->(s)", layer::WORKSPACE_WRITE),
            params! { "id" => project.id },
        )
        .await?;
        Ok(())
    }

    /// The project record and its settings (its sessions are deleted first, by the caller).
    pub async fn delete_project(&self, id: &str) -> Result<(), RpcError> {
        self.run(&format!("{} MATCH (s:Settings {{scope: $id}}) DETACH DELETE s", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        self.run(&format!("{} MATCH (p:Project {{id: $id}}) DETACH DELETE p", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        Ok(())
    }

    pub async fn sessions(&self) -> Result<Vec<Session>, RpcError> {
        self.docs("Session").await
    }

    pub async fn put_session(&self, session: &Session) -> Result<(), RpcError> {
        self.run(
            &format!(
                "{} MERGE (s:Session {{id: $id}}) SET s.project_id = $project, s.parent = $parent, s.topic = $topic, s.title = $title, s.updated_ms = $updated, s.doc = $doc",
                layer::WORKSPACE_WRITE
            ),
            params! {
                "id" => session.id,
                "project" => session.project_id,
                "parent" => session.parent.clone().unwrap_or_default(),
                "topic" => topic(&session.id),
                "title" => session.title,
                "updated" => session.updated_ms,
                "doc" => doc(session)?,
            },
        )
        .await?;
        self.graph.ensure_index(&self.ig_id, "app_session_id", "Session", "id").await?;
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
