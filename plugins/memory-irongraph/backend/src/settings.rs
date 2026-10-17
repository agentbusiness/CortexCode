//! Settings in the main project's WORKSPACE layer, as real graph structure:
//! `(:Settings)-[:SECTION]->(:SettingsSection {name, path})-[:SECTION]->(nested sections)` and
//! `(:SettingsSection)-[:HAS]->(:Setting {key, path, value, type, description, updated_ms, updated_by})`.
//! `value` holds the JSON text of the value; `type` its JSON type, fixed once set.

use crate::db::{Graph, layer};
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::SettingValue;
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct Settings {
    graph: Arc<Graph>,
    ig_id: String,
    /// Serialises writes (section creation is check-then-create).
    write: Mutex<bool>,
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

/// Dotted path segments; each is a non-empty identifier of `[A-Za-z0-9_-]`.
fn segments(path: &str) -> Result<Vec<&str>, RpcError> {
    let parts: Vec<&str> = path.split('.').collect();
    if path.is_empty() || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
        return Err(RpcError::invalid_params(format!("invalid settings path `{path}`: dotted segments of letters, digits, `_` and `-`")));
    }
    Ok(parts)
}

fn s(row: &[Value], i: usize) -> String {
    row.get(i).and_then(|v| v.as_str()).unwrap_or("").to_owned()
}

impl Settings {
    pub async fn open(graph: Arc<Graph>, ig_id: String) -> Result<Self, RpcError> {
        graph.run(&ig_id, &format!("{} MERGE (r:Settings {{id: 'root'}}) SET r.name = 'settings'", layer::WORKSPACE_WRITE), Map::new()).await?;
        graph.ensure_index(&ig_id, "settings_root_id", "Settings", "id").await?;
        Ok(Self { graph, ig_id, write: Mutex::new(false) })
    }

    async fn run(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    fn from_row(row: &[Value]) -> Result<SettingValue, RpcError> {
        let text = s(row, 1);
        let value: Value = serde_json::from_str(&text).map_err(|e| RpcError::internal(format!("stored setting is not JSON: {e}")))?;
        Ok(SettingValue {
            path: s(row, 0),
            value,
            value_type: s(row, 2),
            description: row.get(3).and_then(|v| v.as_str()).map(str::to_owned),
            updated_ms: row.get(4).and_then(|v| v.as_u64()).unwrap_or(0),
            updated_by: s(row, 5),
        })
    }

    pub async fn get(&self, path: &str) -> Result<SettingValue, RpcError> {
        segments(path)?;
        let rows = self
            .run(
                &format!("{} MATCH (x:Setting {{path: $p}}) RETURN x.path, x.value, x.type, x.description, x.updated_ms, x.updated_by", layer::WORKSPACE_READ),
                params! { "p" => path },
            )
            .await?;
        let row = rows.rows.first().ok_or_else(|| RpcError::not_found(format!("no setting `{path}`")))?;
        Self::from_row(row)
    }

    pub async fn set(&self, path: &str, value: Value, by: &str, description: Option<String>) -> Result<SettingValue, RpcError> {
        let parts = segments(path)?;
        if parts.len() < 2 {
            return Err(RpcError::invalid_params(format!("`{path}` names no section: settings live in sections (`section.key`)")));
        }
        if value.is_null() {
            return Err(RpcError::invalid_params("a setting's value cannot be null; delete it instead"));
        }
        let mut indexes = self.write.lock().await;
        let value_type = json_type(&value);
        // The path must not already be a section, and no prefix may be a setting.
        let section = self.run(&format!("{} MATCH (s:SettingsSection {{path: $p}}) RETURN s.path", layer::WORKSPACE_READ), params! { "p" => path }).await?;
        if !section.rows.is_empty() {
            return Err(RpcError::invalid_params(format!("`{path}` is a section, not a setting")));
        }
        for i in 1..parts.len() {
            let prefix = parts[..i].join(".");
            let clash = self.run(&format!("{} MATCH (x:Setting {{path: $p}}) RETURN x.path", layer::WORKSPACE_READ), params! { "p" => prefix }).await?;
            if !clash.rows.is_empty() {
                return Err(RpcError::invalid_params(format!("`{prefix}` is a setting, so it cannot contain `{path}`")));
            }
        }
        let existing = self.run(&format!("{} MATCH (x:Setting {{path: $p}}) RETURN x.type", layer::WORKSPACE_READ), params! { "p" => path }).await?;
        if let Some(row) = existing.rows.first() {
            let stored = s(row, 0);
            if stored != value_type {
                return Err(RpcError::invalid_params(format!("`{path}` holds a {stored}; a {value_type} is rejected (delete the setting to change its type)")));
            }
        }
        // Sections on demand, linked from the root (or their parent section).
        for i in 1..parts.len() {
            let section_path = parts[..i].join(".");
            if i == 1 {
                self.run(
                    &format!("{} MATCH (r:Settings {{id: 'root'}}) MERGE (s:SettingsSection {{path: $p}}) SET s.name = $name MERGE (r)-[:SECTION]->(s)", layer::WORKSPACE_WRITE),
                    params! { "p" => section_path, "name" => parts[i - 1] },
                )
                .await?;
            } else {
                let parent = parts[..i - 1].join(".");
                self.run(
                    &format!("{} MATCH (parent:SettingsSection {{path: $parent}}) MERGE (s:SettingsSection {{path: $p}}) SET s.name = $name MERGE (parent)-[:SECTION]->(s)", layer::WORKSPACE_WRITE),
                    params! { "parent" => parent, "p" => section_path, "name" => parts[i - 1] },
                )
                .await?;
            }
        }
        let section_path = parts[..parts.len() - 1].join(".");
        let key = parts[parts.len() - 1];
        let text = serde_json::to_string(&value).map_err(|e| RpcError::internal(e.to_string()))?;
        let description_set = if description.is_some() { ", x.description = $description" } else { "" };
        self.run(
            &format!(
                "{} MATCH (s:SettingsSection {{path: $section}}) MERGE (x:Setting {{path: $p}}) SET x.key = $key, x.name = $key, x.value = $value, x.type = $type, x.updated_ms = $now, x.updated_by = $by{description_set} MERGE (s)-[:HAS]->(x)",
                layer::WORKSPACE_WRITE
            ),
            params! { "section" => section_path, "p" => path, "key" => key, "value" => text, "type" => value_type, "now" => now_ms(), "by" => by, "description" => description },
        )
        .await?;
        if !*indexes {
            self.graph.ensure_index(&self.ig_id, "setting_path", "Setting", "path").await?;
            self.graph.ensure_index(&self.ig_id, "settings_section_path", "SettingsSection", "path").await?;
            *indexes = true;
        }
        drop(indexes);
        self.get(path).await
    }

    /// Every setting under `prefix` as nested JSON.
    pub async fn list(&self, prefix: &str) -> Result<Value, RpcError> {
        if !prefix.is_empty() {
            segments(prefix)?;
        }
        let rows = self.run(&format!("{} MATCH (r:Settings)-[:SECTION*1..32]->(s:SettingsSection)-[:HAS]->(x:Setting) RETURN DISTINCT x.path, x.value", layer::WORKSPACE_READ), Map::new()).await?;
        let mut root = Map::new();
        for row in &rows.rows {
            let path = s(row, 0);
            if !prefix.is_empty() && path != prefix && !path.starts_with(&format!("{prefix}.")) {
                continue;
            }
            let value: Value = serde_json::from_str(&s(row, 1)).unwrap_or(Value::Null);
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
    pub async fn delete(&self, path: &str) -> Result<Vec<String>, RpcError> {
        segments(path)?;
        let _guard = self.write.lock().await;
        let setting = self.run(&format!("{} MATCH (x:Setting {{path: $p}}) RETURN x.path", layer::WORKSPACE_READ), params! { "p" => path }).await?;
        if !setting.rows.is_empty() {
            self.run(&format!("{} MATCH (x:Setting {{path: $p}}) DETACH DELETE x", layer::WORKSPACE_WRITE), params! { "p" => path }).await?;
            return Ok(vec![path.to_owned()]);
        }
        let section = self.run(&format!("{} MATCH (s:SettingsSection {{path: $p}}) RETURN s.path", layer::WORKSPACE_READ), params! { "p" => path }).await?;
        if section.rows.is_empty() {
            return Err(RpcError::not_found(format!("no setting or section `{path}`")));
        }
        let under = format!("{path}.");
        let settings = self.run(&format!("{} MATCH (x:Setting) RETURN x.path", layer::WORKSPACE_READ), Map::new()).await?;
        let doomed: Vec<String> = settings.column(0).filter_map(|v| v.as_str()).filter(|p| p.starts_with(&under)).map(str::to_owned).collect();
        let sections = self.run(&format!("{} MATCH (s:SettingsSection) RETURN s.path", layer::WORKSPACE_READ), Map::new()).await?;
        let mut doomed_sections: Vec<String> = sections.column(0).filter_map(|v| v.as_str()).filter(|p| p.starts_with(&under)).map(str::to_owned).collect();
        doomed_sections.push(path.to_owned());
        self.run(&format!("{} UNWIND $paths AS p MATCH (x:Setting {{path: p}}) DETACH DELETE x", layer::WORKSPACE_WRITE), params! { "paths" => doomed }).await?;
        self.run(&format!("{} UNWIND $paths AS p MATCH (s:SettingsSection {{path: p}}) DETACH DELETE s", layer::WORKSPACE_WRITE), params! { "paths" => doomed_sections }).await?;
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
