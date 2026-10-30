//! The embedded IronGraph instance: one per process, exclusive owner of `<CORTEX_HOME>/irongraph`.
//! Its embedding model lives in the models folder the Cortex apps share (`~/.cortex-models`) and its
//! vector cache in `<CORTEX_HOME>/irongraph-cache` (see `engine`); nothing is read from or written
//! to `~/.irongraph`.
//!
//! Every statement names its layers explicitly (see the `layer` constants): a statement without a
//! `USE LAYER` reads the default view, which silently excludes WORKSPACE.

use cc_protocol::RpcError;
use crate::engine::EnginePaths;
use irongraph_sdk::{EmbeddedDatabase, EmbeddedOptions, EmbeddingPolicy, OperationOptions, Query, StreamAppend, StreamFetch, StreamRecord};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Layer prefixes. Writes name both the readable layers and the write layer.
pub mod layer {
    pub const OBSERVED_READ: &str = "USE LAYER OBSERVED";
    pub const OBSERVED_WRITE: &str = "USE LAYER OBSERVED WRITE LAYER OBSERVED";
    pub const CODE_READ: &str = "USE LAYER OBSERVED, KNOWLEDGE";
    pub const KNOWLEDGE_READ: &str = "USE LAYER KNOWLEDGE";
    pub const KNOWLEDGE_WRITE: &str = "USE LAYER OBSERVED, KNOWLEDGE WRITE LAYER KNOWLEDGE";
    pub const ALL_READ: &str = "USE LAYER OBSERVED, KNOWLEDGE, WORKSPACE";
    pub const WORKSPACE_READ: &str = "USE LAYER WORKSPACE";
    pub const WORKSPACE_WRITE: &str = "USE LAYER OBSERVED, KNOWLEDGE, WORKSPACE WRITE LAYER WORKSPACE";
}

pub struct Graph {
    db: EmbeddedDatabase,
    /// IronGraph project display name -> immutable project UUID.
    projects: Mutex<HashMap<String, String>>,
    /// (project UUID, index name) pairs already declared by this process: repeated declarations
    /// skip the statement entirely.
    declared: Mutex<HashSet<(String, String)>>,
    /// Project names whose relationship scope this process has set (`ensure_project`).
    scoped: Mutex<HashSet<String>>,
}

/// A decoded query result: plain JSON values (typed envelopes unwrapped).
#[derive(Debug, Clone, Default)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub updates: u64,
    pub elapsed_us: u64,
}

impl Rows {
    pub fn column(&self, index: usize) -> impl Iterator<Item = &Value> {
        self.rows.iter().filter_map(move |row| row.get(index))
    }
}

pub fn ig_error(error: irongraph_sdk::Error) -> RpcError {
    let mut e = RpcError::internal(format!("irongraph {}: {}", error.code, error.message));
    if error.retryable {
        e = e.retryable();
    }
    e
}

impl Graph {
    /// Open the database in `paths.data_dir` (created when missing) with IronGraph's automatic
    /// embedding, its model and vector cache in `paths.model_dir` / `paths.cache_dir`. The model must
    /// already be installed there (`engine::Engine::install`); no embedder is added by the plugin.
    /// `embeddings: false` opens without the embedding model (graph-only use).
    pub async fn open(paths: &EnginePaths, embeddings: bool) -> Result<Arc<Self>, RpcError> {
        for dir in [&paths.data_dir, &paths.model_dir, &paths.cache_dir] {
            std::fs::create_dir_all(dir).map_err(|e| RpcError::internal(format!("cannot create {}: {e}", dir.display())))?;
        }
        let options = EmbeddedOptions::new(&paths.data_dir)
            .with_model_dir(&paths.model_dir)
            .with_embedding_cache_dir(&paths.cache_dir)
            .with_embedding_policy(if embeddings { EmbeddingPolicy::Automatic } else { EmbeddingPolicy::Disabled });
        let db = EmbeddedDatabase::open_async(options).await.map_err(ig_error)?;
        Ok(Arc::new(Self { db, projects: Mutex::new(HashMap::new()), declared: Mutex::new(HashSet::new()), scoped: Mutex::new(HashSet::new()) }))
    }

    /// Close the database when this is its last holder, so the next open needs no repair (an
    /// unclosed one costs a scan of the whole file). False when another holder keeps it open.
    pub fn close(self: Arc<Self>) -> Result<bool, RpcError> {
        match Arc::try_unwrap(self) {
            Ok(graph) => graph.db.close().map(|()| true).map_err(ig_error),
            Err(_) => Ok(false),
        }
    }

    pub async fn flush(&self) -> Result<(), RpcError> {
        self.db.flush_async().await.map_err(ig_error)
    }

    /// The UUID of project `name`, creating it when missing. Its relationships stay out of the
    /// automatic semantic index: every search here binds nodes, and edges outnumber them several
    /// times over. Repeating the setting is a no-op; an older project drops its edge vectors.
    pub async fn ensure_project(&self, name: &str) -> Result<String, RpcError> {
        let known = self.projects.lock().await.get(name).cloned();
        let id = match known {
            Some(id) => id,
            None => {
                validate_project_name(name)?;
                self.db.query_async(Query::new(format!("CREATE PROJECT IF NOT EXISTS {name}"))).await.map_err(ig_error)?;
                self.refresh_projects().await?;
                self.projects.lock().await.get(name).cloned().ok_or_else(|| RpcError::internal(format!("project {name} missing after creation")))?
            }
        };
        if !self.scoped.lock().await.contains(name) {
            self.db.query_async(Query::new(format!("USE {name} ALTER AUTOMATIC SEMANTIC SET RELATIONSHIPS NONE"))).await.map_err(ig_error)?;
            self.scoped.lock().await.insert(name.to_owned());
        }
        Ok(id)
    }

    /// The UUID of an existing project, if any.
    pub async fn project_id(&self, name: &str) -> Result<Option<String>, RpcError> {
        if let Some(id) = self.projects.lock().await.get(name) {
            return Ok(Some(id.clone()));
        }
        self.refresh_projects().await?;
        Ok(self.projects.lock().await.get(name).cloned())
    }

    /// The names of every IronGraph project in the database.
    pub async fn project_names(&self) -> Result<Vec<String>, RpcError> {
        self.refresh_projects().await?;
        Ok(self.projects.lock().await.keys().cloned().collect())
    }

    async fn refresh_projects(&self) -> Result<(), RpcError> {
        let result = self.db.query_async(Query::new("SHOW PROJECTS")).await.map_err(ig_error)?;
        let mut map = self.projects.lock().await;
        map.clear();
        for row in &result.rows {
            if let (Some(id), Some(name)) = (row.first().map(plain), row.get(1).map(plain))
                && let (Some(id), Some(name)) = (id.as_str(), name.as_str())
            {
                map.insert(name.to_owned(), id.to_owned());
            }
        }
        Ok(())
    }

    pub async fn drop_project(&self, name: &str) -> Result<(), RpcError> {
        validate_project_name(name)?;
        if self.project_id(name).await?.is_none() {
            return Ok(());
        }
        self.db.query_async(Query::new(format!("DROP PROJECT {name} CASCADE"))).await.map_err(ig_error)?;
        self.projects.lock().await.remove(name);
        self.scoped.lock().await.remove(name);
        Ok(())
    }

    /// Run one statement against a project with named parameters.
    pub async fn run(&self, project: &str, cypher: &str, params: Map<String, Value>) -> Result<Rows, RpcError> {
        self.run_with(project, cypher, params, OperationOptions::default()).await
    }

    /// [`Graph::run`] that the engine stops after `timeout_ms` (an error that says so).
    pub async fn run_timed(&self, project: &str, cypher: &str, params: Map<String, Value>, timeout_ms: u64) -> Result<Rows, RpcError> {
        self.run_with(project, cypher, params, OperationOptions { operation_id: None, timeout_ms: Some(timeout_ms) }).await
    }

    async fn run_with(&self, project: &str, cypher: &str, params: Map<String, Value>, options: OperationOptions) -> Result<Rows, RpcError> {
        let mut query = Query::new(cypher).with_project(project);
        for (key, value) in params {
            query = query.with_parameter(key, value);
        }
        let started = std::time::Instant::now();
        let head: String = cypher.chars().take(140).collect();
        tracing::debug!(statement = %head, "graph statement");
        let result = self.db.query_with_options_async(query, options).await;
        let elapsed = started.elapsed();
        if elapsed.as_millis() > 2000 {
            tracing::warn!(ms = elapsed.as_millis() as u64, statement = %head, "slow graph statement");
        }
        // A pattern naming a label / type / property no write has declared yet matches nothing:
        // IronGraph reports it as a type error; it is answered as the empty result it is.
        if let Err(e) = &result
            && e.message.contains("is not declared")
            && !cypher.trim_start().to_uppercase().starts_with("CREATE INDEX")
        {
            return Ok(Rows::default());
        }
        let result = result.map_err(|e| {
            let mut error = ig_error(e);
            let head: String = cypher.chars().take(160).collect();
            error.message = format!("{} [statement: {head}]", error.message);
            error
        })?;
        let columns = result.columns.iter().map(|c| c["name"].as_str().unwrap_or("").to_owned()).collect();
        let rows = result.rows.iter().map(|row| row.iter().map(plain).collect()).collect();
        let stats = &result.summary["statistics"];
        Ok(Rows { columns, rows, updates: stats["updates"].as_u64().unwrap_or(0), elapsed_us: stats["elapsed_us"].as_u64().unwrap_or(0) })
    }

    /// Declare an equality index with IronGraph's idempotent form (`CREATE INDEX … IF NOT EXISTS`,
    /// irongraph 0.1.8): an existing name succeeds with zero updates and leaves the index unchanged;
    /// concurrent declarations of one name are resolved by the engine. Names this process already
    /// declared skip the round trip. A label that does not exist yet is skipped (not an error).
    pub async fn ensure_index(&self, project: &str, name: &str, label: &str, property: &str) -> Result<(), RpcError> {
        let key = (project.to_owned(), name.to_owned());
        if self.declared.lock().await.contains(&key) {
            return Ok(());
        }
        match self.run(project, &format!("CREATE INDEX {name} IF NOT EXISTS FOR (n:{label}) ON (n.{property})"), Map::new()).await {
            Ok(_) => {}
            // A label no write has made yet cannot be indexed: the next declaration (the next
            // start) makes the index, once the label exists.
            Err(error) if error.message.contains("is not declared") => return Ok(()),
            Err(error) => return Err(error),
        }
        self.declared.lock().await.insert(key);
        Ok(())
    }

    /// Append records to a stream (one partition), creating its topic on first use. Returns the
    /// offset after the last record.
    pub async fn stream_append(&self, project: &str, topic: &str, records: Vec<Vec<u8>>) -> Result<u64, RpcError> {
        if records.is_empty() {
            return self.stream_end(project, topic).await;
        }
        let request = |records: Vec<Vec<u8>>| StreamAppend {
            project_id: project.to_owned(),
            topic: topic.to_owned(),
            partition: 0,
            records: records.into_iter().map(|value| StreamRecord { key: None, headers: Default::default(), value: Some(value), create_time_ms: None }).collect(),
        };
        let acknowledged = match self.db.stream_append_async(request(records.clone()), OperationOptions::default()).await {
            Ok(acknowledged) => acknowledged,
            Err(error) if missing_topic(&error) => {
                self.create_topic(project, topic).await?;
                self.db.stream_append_async(request(records), OperationOptions::default()).await.map_err(ig_error)?
            }
            Err(error) => return Err(ig_error(error)),
        };
        Ok(acknowledged.first_offset + u64::from(acknowledged.record_count))
    }

    /// Records of a stream from `offset`: each record's offset and bytes, the offset after them,
    /// and the offset after the last record of the stream. A topic that does not exist is empty.
    pub async fn stream_fetch(&self, project: &str, topic: &str, offset: u64, max_records: usize, max_bytes: usize) -> Result<(Vec<(u64, Vec<u8>)>, u64, u64), RpcError> {
        let request = StreamFetch { project_id: project.to_owned(), topic: topic.to_owned(), partition: 0, offset, max_records, max_bytes };
        match self.db.stream_fetch_async(request, OperationOptions::default()).await {
            Ok(page) => {
                let records = page.records.into_iter().map(|(offset, payload)| (offset, payload.payload)).collect();
                Ok((records, page.next_offset, page.high_watermark))
            }
            Err(error) if missing_topic(&error) => Ok((Vec::new(), offset, offset)),
            Err(error) => Err(ig_error(error)),
        }
    }

    /// [`Graph::stream_fetch`] with each record's append time (epoch ms, as the engine resolved it).
    pub async fn stream_page(&self, project: &str, topic: &str, offset: u64, max_records: usize, max_bytes: usize) -> Result<(Vec<(u64, u64, Vec<u8>)>, u64, u64), RpcError> {
        let request = StreamFetch { project_id: project.to_owned(), topic: topic.to_owned(), partition: 0, offset, max_records, max_bytes };
        match self.db.stream_fetch_async(request, OperationOptions::default()).await {
            Ok(page) => {
                let records = page.records.into_iter().map(|(offset, payload)| (offset, payload.resolved_time_ms.max(0) as u64, payload.payload)).collect();
                Ok((records, page.next_offset, page.high_watermark))
            }
            Err(error) if missing_topic(&error) => Ok((Vec::new(), offset, offset)),
            Err(error) => Err(ig_error(error)),
        }
    }

    /// Create a one-partition topic that keeps its records `retention_days` (for good when
    /// `None`). One that exists already is left as it is.
    pub async fn create_topic_kept(&self, project: &str, topic: &str, retention_days: Option<u32>) -> Result<(), RpcError> {
        validate_topic(topic)?;
        let retention = retention_days.map(|days| format!(" RETENTION {days} DAYS")).unwrap_or_default();
        match self.run(project, &format!("CREATE TOPIC `{topic}` PARTITIONS 1{retention}"), Map::new()).await {
            Ok(_) => Ok(()),
            Err(error) if error.message.to_lowercase().contains("exist") => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Every topic of a project: name, record count, offset after the last record, retention (ms).
    pub async fn topics(&self, project: &str) -> Result<Vec<(String, u64, u64, Option<u64>)>, RpcError> {
        let rows = self.run(project, "SHOW TOPICS", Map::new()).await?;
        let index = |name: &str| rows.columns.iter().position(|column| column == name);
        let (Some(name), count, next, retention) = (index("name"), index("record_count"), index("next_offset"), index("retention_ms")) else { return Ok(Vec::new()) };
        let number = |row: &[Value], at: Option<usize>| at.and_then(|at| row.get(at)).and_then(Value::as_u64);
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| Some((row.get(name)?.as_str()?.to_owned(), number(row, count).unwrap_or(0), number(row, next).unwrap_or(0), number(row, retention))))
            .collect())
    }

    /// The offset after the last record of a stream (0 when it does not exist).
    async fn stream_end(&self, project: &str, topic: &str) -> Result<u64, RpcError> {
        Ok(self.stream_fetch(project, topic, 0, 1, 1).await?.2)
    }

    async fn create_topic(&self, project: &str, topic: &str) -> Result<(), RpcError> {
        validate_topic(topic)?;
        match self.run(project, &format!("CREATE TOPIC `{topic}` PARTITIONS 1"), Map::new()).await {
            Ok(_) => Ok(()),
            // Created meanwhile by a concurrent append.
            Err(error) if error.message.to_lowercase().contains("exist") => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Delete a stream and its records; a topic that does not exist is already gone.
    pub async fn drop_topic(&self, project: &str, topic: &str) -> Result<(), RpcError> {
        validate_topic(topic)?;
        match self.run(project, &format!("DROP TOPIC `{topic}`"), Map::new()).await {
            Ok(_) => Ok(()),
            Err(error) if error.message.to_lowercase().contains("does not exist") || error.message.to_lowercase().contains("not found") => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Engine-owned read-only validation of a statement.
    pub async fn check_read_only(&self, project: &str, cypher: &str) -> Result<(), RpcError> {
        let query = Query::new("CHECK READ ONLY").with_project(project).with_parameter("statement", cypher);
        self.db.query_async(query).await.map(|_| ()).map_err(|e| RpcError::invalid_params(format!("statement is not read-only: {}: {}", e.code, e.message)))
    }
}

/// A stream error that means its topic does not exist yet.
fn missing_topic(error: &irongraph_sdk::Error) -> bool {
    let message = error.message.to_lowercase();
    message.contains("does not exist") || message.contains("unknown topic") || message.contains("not found")
}

/// Topic names are quoted identifiers in statements: letters, digits, `.`, `_` and `-` only.
fn validate_topic(topic: &str) -> Result<(), RpcError> {
    if topic.is_empty() || topic.len() > 200 || !topic.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return Err(RpcError::internal(format!("invalid topic name {topic}")));
    }
    Ok(())
}

/// Project names are identifiers in Cypher statements; only `[a-z0-9_]` is accepted.
fn validate_project_name(name: &str) -> Result<(), RpcError> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') || name.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(RpcError::internal(format!("invalid project name {name}")));
    }
    Ok(())
}

/// Unwrap IronGraph's typed value envelopes into plain JSON. Integers arrive as decimal strings and
/// become numbers; nodes become `{id, labels, properties}`; relationships keep type and endpoints.
pub fn plain(value: &Value) -> Value {
    let Some(kind) = value.get("type").and_then(|t| t.as_str()) else { return value.clone() };
    let inner = value.get("value").cloned().unwrap_or(Value::Null);
    match kind {
        "null" => Value::Null,
        "integer" => match &inner {
            Value::String(s) => s.parse::<i64>().map(Value::from).unwrap_or(Value::String(s.clone())),
            other => other.clone(),
        },
        "list" => Value::Array(inner.as_array().map(|items| items.iter().map(plain).collect()).unwrap_or_default()),
        "map" => match inner {
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), plain(v))).collect()),
            other => other,
        },
        "node" => {
            let mut out = Map::new();
            out.insert("id".into(), inner["id"].clone());
            out.insert("labels".into(), inner["labels"].clone());
            let props: Map<String, Value> = inner["properties"].as_object().map(|p| p.iter().map(|(k, v)| (k.clone(), plain(v))).collect()).unwrap_or_default();
            out.insert("properties".into(), Value::Object(props));
            Value::Object(out)
        }
        "relationship" => {
            let mut out = Map::new();
            for key in ["id", "type", "source", "target", "start", "end"] {
                if let Some(v) = inner.get(key) {
                    out.insert(key.into(), plain(v));
                }
            }
            let props: Map<String, Value> = inner["properties"].as_object().map(|p| p.iter().map(|(k, v)| (k.clone(), plain(v))).collect()).unwrap_or_default();
            out.insert("properties".into(), Value::Object(props));
            Value::Object(out)
        }
        "path" => match inner {
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), if v.is_array() { Value::Array(v.as_array().map(|a| a.iter().map(plain).collect()).unwrap_or_default()) } else { plain(v) })).collect()),
            other => other,
        },
        _ => inner,
    }
}

/// Build a parameter map.
#[macro_export]
macro_rules! params {
    ($($key:expr => $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut map = serde_json::Map::new();
        $( map.insert($key.to_string(), serde_json::json!($value)); )*
        map
    }};
}
