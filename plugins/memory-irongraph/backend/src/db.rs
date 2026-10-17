//! The embedded IronGraph instance: one per process, exclusive owner of `<CORTEX_HOME>/irongraph`.
//! Its embedding model and vector cache live in `<CORTEX_HOME>/irongraph-models` and
//! `<CORTEX_HOME>/irongraph-cache` (see `engine`); nothing is read from or written to `~/.irongraph`.
//!
//! Every statement names its layers explicitly (see the `layer` constants): a statement without a
//! `USE LAYER` reads the default view, which silently excludes WORKSPACE.

use cc_protocol::RpcError;
use crate::engine::EnginePaths;
use irongraph_sdk::{EmbeddedDatabase, EmbeddedOptions, EmbeddingPolicy, OperationOptions, Query};
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
        Ok(Arc::new(Self { db, projects: Mutex::new(HashMap::new()), declared: Mutex::new(HashSet::new()) }))
    }

    pub async fn close(self: Arc<Self>) -> Result<(), RpcError> {
        match Arc::try_unwrap(self) {
            Ok(graph) => graph.db.close_async().await.map_err(ig_error),
            Err(shared) => shared.db.flush_async().await.map_err(ig_error),
        }
    }

    pub async fn flush(&self) -> Result<(), RpcError> {
        self.db.flush_async().await.map_err(ig_error)
    }

    /// The UUID of project `name`, creating it when missing.
    pub async fn ensure_project(&self, name: &str) -> Result<String, RpcError> {
        if let Some(id) = self.projects.lock().await.get(name) {
            return Ok(id.clone());
        }
        validate_project_name(name)?;
        self.db.query_async(Query::new(format!("CREATE PROJECT IF NOT EXISTS {name}"))).await.map_err(ig_error)?;
        self.refresh_projects().await?;
        self.projects.lock().await.get(name).cloned().ok_or_else(|| RpcError::internal(format!("project {name} missing after creation")))
    }

    /// The UUID of an existing project, if any.
    pub async fn project_id(&self, name: &str) -> Result<Option<String>, RpcError> {
        if let Some(id) = self.projects.lock().await.get(name) {
            return Ok(Some(id.clone()));
        }
        self.refresh_projects().await?;
        Ok(self.projects.lock().await.get(name).cloned())
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
        Ok(())
    }

    /// Run one statement against a project with named parameters.
    pub async fn run(&self, project: &str, cypher: &str, params: Map<String, Value>) -> Result<Rows, RpcError> {
        let mut query = Query::new(cypher).with_project(project);
        for (key, value) in params {
            query = query.with_parameter(key, value);
        }
        let started = std::time::Instant::now();
        let head: String = cypher.chars().take(140).collect();
        tracing::debug!(statement = %head, "graph statement");
        let result = self.db.query_with_options_async(query, OperationOptions::default()).await;
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
    /// declared skip the round trip.
    pub async fn ensure_index(&self, project: &str, name: &str, label: &str, property: &str) -> Result<(), RpcError> {
        let key = (project.to_owned(), name.to_owned());
        if self.declared.lock().await.contains(&key) {
            return Ok(());
        }
        self.run(project, &format!("CREATE INDEX {name} IF NOT EXISTS FOR (n:{label}) ON (n.{property})"), Map::new()).await?;
        self.declared.lock().await.insert(key);
        Ok(())
    }

    /// Engine-owned read-only validation of a statement.
    pub async fn check_read_only(&self, project: &str, cypher: &str) -> Result<(), RpcError> {
        let query = Query::new("CHECK READ ONLY").with_project(project).with_parameter("statement", cypher);
        self.db.query_async(query).await.map(|_| ()).map_err(|e| RpcError::invalid_params(format!("statement is not read-only: {}: {}", e.code, e.message)))
    }
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
