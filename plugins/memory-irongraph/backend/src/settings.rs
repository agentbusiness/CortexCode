//! Settings in the main project's WORKSPACE layer: one node per scope holds all of its settings,
//! `(:Settings {scope: 'global'})` for the app and `(:Settings {scope: <project id>})` for a project
//! (linked from its `(:Project)`). Its `values` property holds the JSON text of a map from each
//! setting's dotted path to `{value, type, description, updated_ms, updated_by}`; a value's JSON
//! type is fixed once set. Paths are sections then the key (`model.variant`).

use crate::db::{Graph, layer};
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::SettingValue;
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

/// The app's own scope (every other scope is a project id).
pub const GLOBAL: &str = "global";

pub struct Settings {
    graph: Arc<Graph>,
    ig_id: String,
    /// Serialises writes: each one reads a scope's map, changes it and writes it back.
    write: Mutex<()>,
}

/// JSON type name of a value (integers and floats are both `number`).
pub fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The scope a caller named: empty means the app's own.
fn scope_of(scope: &str) -> &str {
    if scope.is_empty() { GLOBAL } else { scope }
}

/// Dotted path segments; each is a non-empty identifier of `[A-Za-z0-9_-]`.
fn segments(path: &str) -> Result<Vec<&str>, RpcError> {
    let parts: Vec<&str> = path.split('.').collect();
    if path.is_empty() || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
        return Err(RpcError::invalid_params(format!("invalid settings path `{path}`: dotted segments of letters, digits, `_` and `-`")));
    }
    Ok(parts)
}

fn setting(path: &str, entry: &Value) -> SettingValue {
    SettingValue {
        path: path.to_owned(),
        value: entry.get("value").cloned().unwrap_or(Value::Null),
        value_type: entry.get("type").and_then(Value::as_str).unwrap_or("").to_owned(),
        description: entry.get("description").and_then(Value::as_str).map(str::to_owned),
        updated_ms: entry.get("updated_ms").and_then(Value::as_u64).unwrap_or(0),
        updated_by: entry.get("updated_by").and_then(Value::as_str).unwrap_or("").to_owned(),
    }
}

impl Settings {
    pub async fn open(graph: Arc<Graph>, ig_id: String) -> Result<Self, RpcError> {
        let settings = Self { graph, ig_id, write: Mutex::new(()) };
        settings.run(&format!("{} MERGE (s:Settings {{scope: $scope}})", layer::WORKSPACE_WRITE), params! { "scope" => GLOBAL }).await?;
        settings.graph.ensure_index(&settings.ig_id, "settings_scope", "Settings", "scope").await?;
        Ok(settings)
    }

    async fn run(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    /// A scope's settings: path -> entry (empty when the scope has none).
    async fn load(&self, scope: &str) -> Result<Map<String, Value>, RpcError> {
        let rows = self.run(&format!("{} MATCH (s:Settings {{scope: $scope}}) RETURN s.values", layer::WORKSPACE_READ), params! { "scope" => scope }).await?;
        let Some(text) = rows.rows.first().and_then(|row| row.first()).and_then(Value::as_str) else { return Ok(Map::new()) };
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => Ok(map),
            Ok(_) | Err(_) => Err(RpcError::internal(format!("the settings of `{scope}` are not a JSON object"))),
        }
    }

    /// Write a scope's settings back; a project's node is linked from its project.
    async fn store(&self, scope: &str, values: &Map<String, Value>) -> Result<(), RpcError> {
        let text = serde_json::to_string(values).map_err(|error| RpcError::internal(error.to_string()))?;
        self.run(
            &format!("{} MERGE (s:Settings {{scope: $scope}}) SET s.values = $values, s.updated_ms = $now", layer::WORKSPACE_WRITE),
            params! { "scope" => scope, "values" => text, "now" => now_ms() },
        )
        .await?;
        if scope != GLOBAL {
            self.run(
                &format!("{} MATCH (p:Project {{id: $scope}}), (s:Settings {{scope: $scope}}) MERGE (p)-[:HAS_SETTINGS]->(s)", layer::WORKSPACE_WRITE),
                params! { "scope" => scope },
            )
            .await?;
        }
        Ok(())
    }

    pub async fn get(&self, scope: &str, path: &str) -> Result<SettingValue, RpcError> {
        segments(path)?;
        let values = self.load(scope_of(scope)).await?;
        values.get(path).map(|entry| setting(path, entry)).ok_or_else(|| RpcError::not_found(format!("no setting `{path}`")))
    }

    pub async fn set(&self, scope: &str, path: &str, value: Value, by: &str, description: Option<String>) -> Result<SettingValue, RpcError> {
        let parts = segments(path)?;
        if parts.len() < 2 {
            return Err(RpcError::invalid_params(format!("`{path}` names no section: settings live in sections (`section.key`)")));
        }
        if value.is_null() {
            return Err(RpcError::invalid_params("a setting's value cannot be null; delete it instead"));
        }
        let scope = scope_of(scope);
        let _write = self.write.lock().await;
        let mut values = self.load(scope).await?;
        let value_type = json_type(&value);
        // The path must not be a section (a prefix of other settings), and no prefix may be a setting.
        let under = format!("{path}.");
        if values.keys().any(|key| key.starts_with(&under)) {
            return Err(RpcError::invalid_params(format!("`{path}` is a section, not a setting")));
        }
        for i in 1..parts.len() {
            let prefix = parts[..i].join(".");
            if values.contains_key(&prefix) {
                return Err(RpcError::invalid_params(format!("`{prefix}` is a setting, so it cannot contain `{path}`")));
            }
        }
        let previous = values.get(path).cloned();
        if let Some(stored) = previous.as_ref().and_then(|entry| entry.get("type")).and_then(Value::as_str)
            && stored != value_type
        {
            return Err(RpcError::invalid_params(format!("`{path}` holds a {stored}; a {value_type} is rejected (delete the setting to change its type)")));
        }
        let description = description.or_else(|| previous.as_ref().and_then(|entry| entry.get("description")).and_then(Value::as_str).map(str::to_owned));
        let entry = json!({ "value": value, "type": value_type, "description": description, "updated_ms": now_ms(), "updated_by": by });
        values.insert(path.to_owned(), entry.clone());
        self.store(scope, &values).await?;
        Ok(setting(path, &entry))
    }

    /// Every setting under `prefix` as nested JSON.
    pub async fn list(&self, scope: &str, prefix: &str) -> Result<Value, RpcError> {
        if !prefix.is_empty() {
            segments(prefix)?;
        }
        let values = self.load(scope_of(scope)).await?;
        let mut root = Map::new();
        for (path, entry) in &values {
            if !prefix.is_empty() && path != prefix && !path.starts_with(&format!("{prefix}.")) {
                continue;
            }
            let value = entry.get("value").cloned().unwrap_or(Value::Null);
            let relative = if prefix.is_empty() { path.as_str() } else { path.strip_prefix(prefix).unwrap_or("").trim_start_matches('.') };
            if relative.is_empty() {
                // The prefix itself is a setting.
                return Ok(value);
            }
            let parts: Vec<&str> = relative.split('.').collect();
            insert_nested(&mut root, &parts, value);
        }
        Ok(Value::Object(root))
    }

    /// Delete a setting, or a section with everything under it. Returns the deleted setting paths.
    pub async fn delete(&self, scope: &str, path: &str) -> Result<Vec<String>, RpcError> {
        segments(path)?;
        let scope = scope_of(scope);
        let _write = self.write.lock().await;
        let mut values = self.load(scope).await?;
        let under = format!("{path}.");
        let doomed: Vec<String> = values.keys().filter(|key| *key == path || key.starts_with(&under)).cloned().collect();
        if doomed.is_empty() {
            return Err(RpcError::not_found(format!("no setting or section `{path}`")));
        }
        for key in &doomed {
            values.remove(key);
        }
        self.store(scope, &values).await?;
        Ok(doomed)
    }
}

fn insert_nested(node: &mut Map<String, Value>, parts: &[&str], value: Value) {
    match parts {
        [] => {}
        [last] => {
            node.insert((*last).to_owned(), value);
        }
        [head, rest @ ..] => {
            let entry = node.entry((*head).to_owned()).or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Value::Object(child) = entry {
                insert_nested(child, rest, value);
            }
        }
    }
}
