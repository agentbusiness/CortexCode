//! Settings in the main project's WORKSPACE layer, as a tree of nodes (shared with CortexTerm):
//!
//! ```text
//! (:Settings {scope, first_ms, last_ms})                            one per scope: `global` or a project id
//!     -[:HAS_SECTION]->(:SettingSection {id, scope, path, name, description, list?, first_ms, last_ms})
//!         -[:HAS_SECTION]->(:SettingSection)                        sections nest
//!         -[:HAS_SETTING]->(:Setting {id, scope, path, name, value_type, text | integer | number |
//!                                      flag | texts | numbers, description, updated_ms, updated_by,
//!                                      first_ms, last_ms})
//! (:Project)-[:HAS_SETTINGS]->(:Settings)                           a project's scope
//! stream `settings`                                                 every change: {scope, path, op, value, by}
//! ```
//!
//! `path` is the list of segments from the scope down (`["model", "variant"]`); `id` is the scope
//! and the segments joined by `\u{1f}`. A setting holds one typed value: `string` (`text`),
//! `number` (`integer` when whole, else `number`), `boolean` (`flag`), `array` of strings
//! (`texts`) or of numbers (`numbers`), or `null` (inside an object or a list only). An object value
//! is stored as a section with one node per member, so nothing is kept as JSON text; reading the
//! section gives the object back. Any other list (of objects, of lists, of mixed values) is a
//! section with `list: true` whose members are named by their index (`0`, `1`, …); reading it
//! gives the list back, in order. Callers name a setting by a dotted path (`model.variant`); the
//! members of an object value may have any non-empty name (`layout.paneCollapsed`, `pane.code@1`).
//! A setting set by its dotted path keeps the JSON type it was first set with.
//!
//! Settings an earlier version kept as one JSON text (`Settings.values`) are read through while
//! they last: a read answers from the tree and that text at once, so no caller waits for a move
//! or sees defaults. A path that is set or deleted leaves the text (a plugin that moves its old
//! record elsewhere reads it, then deletes it, so it never becomes setting nodes); what is left
//! after [`DRAIN_AFTER_MS`] is written into the tree in one batch and the text is removed.
//!
//! Every write is one batch: a few `UNWIND` statements for all the sections, settings and edges
//! it touches, whatever their number.

use crate::db::{Graph, layer};
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::SettingValue;
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::Mutex;

/// The app's own scope (every other scope is a project id).
pub const GLOBAL: &str = "global";
/// The stream every change is appended to.
pub const TOPIC: &str = "settings";
/// Joins a scope and its segments into a node id.
const SEP: char = '\u{1f}';
/// How long the old JSON text is read through before what is left of it moves into the tree.
pub const DRAIN_AFTER_MS: u64 = 10 * 60 * 1000;
/// Rows per write statement.
const CHUNK: usize = 1000;

pub struct Settings {
    graph: Arc<Graph>,
    ig_id: String,
    /// Serialises writes: each one reads a scope's tree, compares and writes the difference.
    write: Mutex<()>,
    /// The old JSON text of each scope (dotted path -> entry), read once; empty when it has none.
    old: Mutex<BTreeMap<String, Map<String, Value>>>,
    /// When this store opened (the old text drains [`DRAIN_AFTER_MS`] later).
    opened_ms: u64,
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
fn segments(path: &str) -> Result<Vec<String>, RpcError> {
    let parts: Vec<&str> = path.split('.').collect();
    if path.is_empty() || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
        return Err(RpcError::invalid_params(format!("invalid settings path `{path}`: dotted segments of letters, digits, `_` and `-`")));
    }
    Ok(parts.into_iter().map(str::to_owned).collect())
}

fn node_id(scope: &str, path: &[String]) -> String {
    let mut id = scope.to_owned();
    for segment in path {
        id.push(SEP);
        id.push_str(segment);
    }
    id
}

fn dotted(path: &[String]) -> String {
    path.join(".")
}

fn under(path: &[String], prefix: &[String]) -> bool {
    path.len() >= prefix.len() && path[..prefix.len()] == *prefix
}

/// One stored setting.
#[derive(Debug, Clone, PartialEq)]
struct Leaf {
    value: Value,
    description: Option<String>,
    updated_ms: u64,
    updated_by: String,
}

/// One stored section.
#[derive(Debug, Clone, Default, PartialEq)]
struct Section {
    description: Option<String>,
    /// A list: its members are named by their index.
    list: bool,
}

/// A scope's tree as read: settings and sections by path. Those only in the old JSON text are
/// marked: they are not nodes yet.
#[derive(Debug, Default)]
struct Tree {
    leaves: BTreeMap<Vec<String>, Leaf>,
    sections: BTreeMap<Vec<String>, Section>,
    old_leaves: BTreeSet<Vec<String>>,
    old_sections: BTreeSet<Vec<String>>,
}

impl Tree {
    fn is_section(&self, path: &[String]) -> bool {
        self.sections.contains_key(path) || self.leaves.keys().any(|key| key.len() > path.len() && under(key, path))
    }

    /// A value rebuilt from the leaves under `path`, with list sections turned back into lists.
    fn shaped(&self, value: Value, path: &[String]) -> Value {
        let Value::Object(members) = value else { return value };
        let mut members: Vec<(String, Value)> = members
            .into_iter()
            .map(|(name, member)| {
                let mut child = path.to_vec();
                child.push(name.clone());
                let member = self.shaped(member, &child);
                (name, member)
            })
            .collect();
        if self.sections.get(path).is_some_and(|section| section.list) {
            members.sort_by_key(|(name, _)| name.parse::<usize>().unwrap_or(usize::MAX));
            Value::Array(members.into_iter().map(|(_, member)| member).collect())
        } else {
            Value::Object(members.into_iter().collect())
        }
    }

    /// The value at `path`: a setting's value, or a section rebuilt as an object (or a list).
    fn value(&self, path: &[String]) -> Option<(Value, u64, String, Option<String>)> {
        if let Some(leaf) = self.leaves.get(path) {
            return Some((leaf.value.clone(), leaf.updated_ms, leaf.updated_by.clone(), leaf.description.clone()));
        }
        if !self.is_section(path) {
            return None;
        }
        let mut root = Map::new();
        let mut updated = (0, String::new());
        for (key, leaf) in self.leaves.range(path.to_vec()..) {
            if !under(key, path) {
                break;
            }
            insert_nested(&mut root, &key[path.len()..], leaf.value.clone());
            if leaf.updated_ms >= updated.0 {
                updated = (leaf.updated_ms, leaf.updated_by.clone());
            }
        }
        let description = self.sections.get(path).and_then(|section| section.description.clone());
        Some((self.shaped(Value::Object(root), path), updated.0, updated.1, description))
    }
}

/// The typed columns of a value: (value_type, column, value).
fn columns(value: &Value) -> Result<(&'static str, &'static str, Value), String> {
    Ok(match value {
        Value::Null => ("null", "", Value::Null),
        Value::Bool(flag) => ("boolean", "flag", json!(flag)),
        Value::String(text) => ("string", "text", json!(text)),
        Value::Number(number) if number.is_i64() || number.is_u64() => ("number", "integer", value.clone()),
        Value::Number(_) => ("number", "number", value.clone()),
        Value::Array(items) if items.iter().all(Value::is_string) => ("array", "texts", value.clone()),
        Value::Array(items) if items.iter().all(Value::is_number) => ("array", "numbers", Value::Array(items.iter().map(|item| json!(item.as_f64().unwrap_or(0.0))).collect())),
        Value::Array(_) => return Err("a list of other values than strings or numbers is a section, not one setting".into()),
        Value::Object(_) => return Err("an object is a section, not one setting".into()),
    })
}

/// A value kept as a section: an object, or a list [`columns`] cannot hold as one setting.
fn structured(value: &Value) -> bool {
    value.is_object() || (value.is_array() && columns(value).is_err())
}

/// Every setting a value becomes (path, member value), and every section it becomes (path, is a
/// list), the value's own path included.
fn flatten(path: &[String], value: &Value, out: &mut Vec<(Vec<String>, Value)>, sections: &mut Vec<(Vec<String>, bool)>) -> Result<(), RpcError> {
    match value {
        Value::Object(members) => {
            sections.push((path.to_vec(), false));
            for (name, member) in members {
                if name.is_empty() {
                    return Err(RpcError::invalid_params(format!("`{}` has a member without a name", dotted(path))));
                }
                let mut child = path.to_vec();
                child.push(name.clone());
                flatten(&child, member, out, sections)?;
            }
            Ok(())
        }
        Value::Array(items) if structured(value) => {
            sections.push((path.to_vec(), true));
            for (index, item) in items.iter().enumerate() {
                let mut child = path.to_vec();
                child.push(index.to_string());
                flatten(&child, item, out, sections)?;
            }
            Ok(())
        }
        other => {
            columns(other).map_err(|error| RpcError::invalid_params(format!("`{}`: {error}", dotted(path))))?;
            out.push((path.to_vec(), other.clone()));
            Ok(())
        }
    }
}

impl Settings {
    pub async fn open(graph: Arc<Graph>, ig_id: String) -> Result<Self, RpcError> {
        let settings = Self { graph, ig_id, write: Mutex::new(()), old: Mutex::new(BTreeMap::new()), opened_ms: now_ms() };
        let now = now_ms();
        settings
            .run(
                &format!("{} MERGE (s:Settings {{scope: $scope}}) ON CREATE SET s.first_ms = $now SET s.last_ms = coalesce(s.last_ms, $now)", layer::WORKSPACE_WRITE),
                params! { "scope" => GLOBAL, "now" => now },
            )
            .await?;
        settings.graph.ensure_index(&settings.ig_id, "settings_scope", "Settings", "scope").await?;
        settings.graph.ensure_index(&settings.ig_id, "setting_id", "Setting", "id").await?;
        settings.graph.ensure_index(&settings.ig_id, "setting_scope", "Setting", "scope").await?;
        settings.graph.ensure_index(&settings.ig_id, "setting_section_id", "SettingSection", "id").await?;
        settings.graph.ensure_index(&settings.ig_id, "setting_section_scope", "SettingSection", "scope").await?;
        Ok(settings)
    }

    async fn run(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    /// The properties of every node of `label` in a scope (whole nodes, so a value column no
    /// setting has yet cannot empty the answer).
    async fn nodes(&self, label: &str, scope: &str) -> Result<Vec<Map<String, Value>>, RpcError> {
        let rows = self.run(&format!("{} MATCH (n:{label} {{scope: $scope}}) RETURN n", layer::WORKSPACE_READ), params! { "scope" => scope }).await?;
        Ok(rows.column(0).filter_map(|node| node.get("properties").and_then(Value::as_object).cloned()).collect())
    }

    /// [`Settings::load`] for a read: past [`DRAIN_AFTER_MS`], what is left of the old JSON text
    /// moves into the tree first.
    async fn read(&self, scope: &str) -> Result<Tree, RpcError> {
        if now_ms().saturating_sub(self.opened_ms) >= DRAIN_AFTER_MS && !self.old_entries(scope).await?.is_empty() {
            let _write = self.write.lock().await;
            let old = self.old_entries(scope).await?;
            if !old.is_empty() {
                let tree = self.load(scope).await?;
                self.drain(scope, &old, &tree).await?;
            }
        }
        self.load(scope).await
    }

    /// A scope's tree: its nodes, with the old JSON text's entries that are not nodes yet laid
    /// under them. Called under the write lock by writers.
    async fn load(&self, scope: &str) -> Result<Tree, RpcError> {
        let mut tree = Tree::default();
        for node in self.nodes("Setting", scope).await? {
            let Some(path) = node.get("path").and_then(strings) else { continue };
            let column = |name: &str| node.get(name).cloned().filter(|value| !value.is_null());
            let value = match node.get("value_type").and_then(Value::as_str).unwrap_or("") {
                "string" => column("text").unwrap_or(Value::Null),
                "number" => column("integer").or_else(|| column("number")).unwrap_or(Value::Null),
                "boolean" => column("flag").unwrap_or(Value::Null),
                "array" => column("texts").or_else(|| column("numbers")).unwrap_or_else(|| json!([])),
                _ => Value::Null,
            };
            let leaf = Leaf {
                value,
                description: node.get("description").and_then(Value::as_str).map(str::to_owned),
                updated_ms: node.get("updated_ms").and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0),
                updated_by: node.get("updated_by").and_then(Value::as_str).unwrap_or("").to_owned(),
            };
            tree.leaves.insert(path, leaf);
        }
        for node in self.nodes("SettingSection", scope).await? {
            if let Some(path) = node.get("path").and_then(strings) {
                let list = node.get("list").and_then(Value::as_bool).unwrap_or(false);
                tree.sections.insert(path, Section { description: node.get("description").and_then(Value::as_str).map(str::to_owned), list });
            }
        }
        let old = self.old_entries(scope).await?;
        for (path, entry) in &old {
            let Ok(path) = segments(path) else { continue };
            let (leaves, sections) = old_entry(&path, entry);
            for (leaf_path, leaf) in leaves {
                if !tree.leaves.contains_key(&leaf_path) {
                    tree.old_leaves.insert(leaf_path.clone());
                    tree.leaves.insert(leaf_path, leaf);
                }
            }
            for (section_path, section) in sections {
                if !tree.sections.contains_key(&section_path) {
                    tree.old_sections.insert(section_path.clone());
                    tree.sections.insert(section_path, section);
                }
            }
        }
        Ok(tree)
    }

    /// The old JSON text of a scope (dotted path -> entry), read from its node once.
    async fn old_entries(&self, scope: &str) -> Result<Map<String, Value>, RpcError> {
        if let Some(entries) = self.old.lock().await.get(scope) {
            return Ok(entries.clone());
        }
        let rows = self.run(&format!("{} MATCH (s:Settings {{scope: $scope}}) RETURN s", layer::WORKSPACE_READ), params! { "scope" => scope }).await?;
        let text = rows.column(0).find_map(|node| node.get("properties").and_then(|props| props.get("values")).and_then(Value::as_str).map(str::to_owned));
        let entries = match text.as_deref().map(serde_json::from_str::<Value>) {
            Some(Ok(Value::Object(map))) => map,
            Some(_) => {
                tracing::warn!(scope, "the old settings text is not a JSON object; it is dropped");
                Map::new()
            }
            None => Map::new(),
        };
        self.old.lock().await.insert(scope.to_owned(), entries.clone());
        Ok(entries)
    }

    /// Drop the old text's entries at or under `path` (they were set or deleted); the text goes
    /// when none is left.
    async fn forget_old(&self, scope: &str, path: &[String]) -> Result<(), RpcError> {
        let mut old = self.old.lock().await;
        let Some(entries) = old.get_mut(scope) else { return Ok(()) };
        let before = entries.len();
        entries.retain(|key, _| segments(key).map(|key| !under(&key, path)).unwrap_or(true));
        if entries.len() == before {
            return Ok(());
        }
        let text = (!entries.is_empty()).then(|| Value::Object(entries.clone()).to_string());
        drop(old);
        self.run(&format!("{} MATCH (s:Settings {{scope: $scope}}) SET s.values = $values", layer::WORKSPACE_WRITE), params! { "scope" => scope, "values" => text }).await?;
        Ok(())
    }

    /// Write what is left of the old text into the tree, one batch, and remove the text (under
    /// the write lock).
    async fn drain(&self, scope: &str, old: &Map<String, Value>, tree: &Tree) -> Result<(), RpcError> {
        let (mut leaves, mut sections) = (Vec::new(), Vec::new());
        for (path, entry) in old {
            let Ok(path) = segments(path) else { continue };
            let (entry_leaves, entry_sections) = old_entry(&path, entry);
            leaves.extend(entry_leaves.into_iter().filter(|(leaf_path, _)| !tree.leaves.contains_key(leaf_path)));
            sections.extend(entry_sections.into_iter().filter(|(section_path, _)| !tree.sections.contains_key(section_path)));
        }
        self.write_batch(scope, &leaves, &sections).await?;
        self.run(&format!("{} MATCH (s:Settings {{scope: $scope}}) REMOVE s.values", layer::WORKSPACE_WRITE), params! { "scope" => scope }).await?;
        self.old.lock().await.insert(scope.to_owned(), Map::new());
        tracing::info!(scope, settings = leaves.len(), "the old settings text moved into the tree");
        Ok(())
    }

    /// The scope's root node, linked from its project.
    async fn ensure_scope(&self, scope: &str) -> Result<(), RpcError> {
        let now = now_ms();
        self.run(
            &format!("{} MERGE (s:Settings {{scope: $scope}}) ON CREATE SET s.first_ms = $now SET s.last_ms = $now", layer::WORKSPACE_WRITE),
            params! { "scope" => scope, "now" => now },
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

    /// Write settings and sections in one batch: every section on their paths (each linked to
    /// its parent), the sections' `list` and description where given, and the settings with their
    /// typed values (the other value columns cleared), each linked to its section.
    async fn write_batch(&self, scope: &str, leaves: &[(Vec<String>, Leaf)], sections: &[(Vec<String>, Section)]) -> Result<(), RpcError> {
        if leaves.is_empty() && sections.is_empty() {
            return Ok(());
        }
        self.ensure_scope(scope).await?;
        let now = now_ms();
        // Every section on the way, parents first.
        let mut all: BTreeSet<Vec<String>> = BTreeSet::new();
        for path in leaves.iter().map(|(path, _)| &path[..path.len() - 1]).chain(sections.iter().map(|(path, _)| path.as_slice())) {
            for depth in 1..=path.len() {
                all.insert(path[..depth].to_vec());
            }
        }
        let rows: Vec<Value> = all.iter().map(|path| json!({ "id": node_id(scope, path), "path": path, "name": path[path.len() - 1] })).collect();
        for chunk in rows.chunks(CHUNK) {
            self.run(
                &format!(
                    "{} UNWIND $rows AS r MERGE (n:SettingSection {{id: r.id}}) ON CREATE SET n.first_ms = $now SET n.scope = $scope, n.path = r.path, n.name = r.name, n.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "rows" => chunk, "scope" => scope, "now" => now },
            )
            .await?;
        }
        let shaped: Vec<Value> = sections.iter().map(|(path, section)| json!({ "id": node_id(scope, path), "list": section.list.then_some(true), "description": section.description })).collect();
        for chunk in shaped.chunks(CHUNK) {
            self.run(&format!("{} UNWIND $rows AS r MATCH (n:SettingSection {{id: r.id}}) SET n.list = r.list, n.description = r.description", layer::WORKSPACE_WRITE), params! { "rows" => chunk }).await?;
        }
        self.link(scope, "SettingSection", "HAS_SECTION", all.iter().map(Vec::as_slice)).await?;
        // The settings, one statement per value column.
        let mut by_column: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
        for (path, leaf) in leaves {
            let (value_type, column, value) = columns(&leaf.value).map_err(RpcError::invalid_params)?;
            by_column.entry(column).or_default().push(json!({
                "id": node_id(scope, path), "path": path, "name": path[path.len() - 1], "type": value_type, "value": value,
                "description": leaf.description, "updated": leaf.updated_ms, "by": leaf.updated_by,
            }));
        }
        for (column, rows) in &by_column {
            let set = ["text", "integer", "number", "flag", "texts", "numbers"]
                .iter()
                .map(|other| if other == column { format!("n.{other} = r.value") } else { format!("n.{other} = null") })
                .collect::<Vec<_>>()
                .join(", ");
            for chunk in rows.chunks(CHUNK) {
                self.run(
                    &format!(
                        "{} UNWIND $rows AS r MERGE (n:Setting {{id: r.id}}) ON CREATE SET n.first_ms = $now SET {set}, n.scope = $scope, n.path = r.path, n.name = r.name, n.value_type = r.type, n.description = r.description, n.updated_ms = r.updated, n.updated_by = r.by, n.last_ms = $now",
                        layer::WORKSPACE_WRITE
                    ),
                    params! { "rows" => chunk, "scope" => scope, "now" => now },
                )
                .await?;
            }
        }
        self.link(scope, "Setting", "HAS_SETTING", leaves.iter().map(|(path, _)| path.as_slice())).await
    }

    /// Link nodes of `label` to their parent: the scope's root for a top-level one, else the
    /// section above.
    async fn link<'a>(&self, scope: &str, label: &str, rel: &str, paths: impl Iterator<Item = &'a [String]>) -> Result<(), RpcError> {
        let (mut top, mut nested) = (Vec::new(), Vec::new());
        for path in paths {
            if path.len() == 1 {
                top.push(json!(node_id(scope, path)));
            } else {
                nested.push(json!({ "id": node_id(scope, path), "parent": node_id(scope, &path[..path.len() - 1]) }));
            }
        }
        for chunk in top.chunks(CHUNK) {
            self.run(&format!("{} MATCH (s:Settings {{scope: $scope}}) UNWIND $ids AS i MATCH (n:{label} {{id: i}}) MERGE (s)-[:{rel}]->(n)", layer::WORKSPACE_WRITE), params! { "scope" => scope, "ids" => chunk })
                .await?;
        }
        for chunk in nested.chunks(CHUNK) {
            self.run(
                &format!("{} UNWIND $rows AS r MATCH (p:SettingSection {{id: r.parent}}) MATCH (n:{label} {{id: r.id}}) MERGE (p)-[:{rel}]->(n)", layer::WORKSPACE_WRITE),
                params! { "rows" => chunk },
            )
            .await?;
        }
        Ok(())
    }

    /// Delete settings and sections by path, with their edges.
    async fn remove(&self, scope: &str, leaves: &[Vec<String>], sections: &[Vec<String>]) -> Result<(), RpcError> {
        if !leaves.is_empty() {
            let ids: Vec<String> = leaves.iter().map(|path| node_id(scope, path)).collect();
            self.run(&format!("{} UNWIND $ids AS id MATCH (n:Setting {{id: id}}) DETACH DELETE n", layer::WORKSPACE_WRITE), params! { "ids" => ids }).await?;
        }
        if !sections.is_empty() {
            let ids: Vec<String> = sections.iter().map(|path| node_id(scope, path)).collect();
            self.run(&format!("{} UNWIND $ids AS id MATCH (n:SettingSection {{id: id}}) DETACH DELETE n", layer::WORKSPACE_WRITE), params! { "ids" => ids }).await?;
        }
        Ok(())
    }

    /// Append changes to the `settings` stream (the shared stream envelope `{at_ms, value}`).
    async fn record(&self, changes: Vec<Value>) {
        if changes.is_empty() {
            return;
        }
        let at = now_ms();
        let records: Vec<Vec<u8>> = changes.into_iter().filter_map(|value| serde_json::to_vec(&json!({ "at_ms": at, "value": value })).ok()).collect();
        if let Err(error) = self.graph.stream_append(&self.ig_id, TOPIC, records).await {
            tracing::warn!(error = %error.message, "settings changes not added to their stream");
        }
    }

    pub async fn get(&self, scope: &str, path: &str) -> Result<SettingValue, RpcError> {
        let parts = segments(path)?;
        let tree = self.read(scope_of(scope)).await?;
        let (value, updated_ms, updated_by, description) = tree.value(&parts).ok_or_else(|| RpcError::not_found(format!("no setting `{path}`")))?;
        Ok(SettingValue { path: path.to_owned(), value_type: json_type(&value).to_owned(), value, description, updated_ms, updated_by })
    }

    pub async fn set(&self, scope: &str, path: &str, value: Value, by: &str, description: Option<String>) -> Result<SettingValue, RpcError> {
        let parts = segments(path)?;
        if parts.len() < 2 {
            return Err(RpcError::invalid_params(format!("`{path}` names no section: settings live in sections (`section.key`)")));
        }
        if value.is_null() {
            return Err(RpcError::invalid_params("a setting's value cannot be null; delete it instead"));
        }
        let (mut flat, mut sections) = (Vec::new(), Vec::new());
        flatten(&parts, &value, &mut flat, &mut sections)?;
        let scope = scope_of(scope);
        let _write = self.write.lock().await;
        let tree = self.load(scope).await?;
        // No prefix of the path may be a setting.
        for i in 1..parts.len() {
            if tree.leaves.contains_key(&parts[..i]) {
                return Err(RpcError::invalid_params(format!("`{}` is a setting, so it cannot contain `{path}`", dotted(&parts[..i]))));
            }
        }
        let is_section = tree.is_section(&parts);
        let is_list = tree.sections.get(&parts).is_some_and(|section| section.list);
        let stored_type = if is_list { Some("array") } else { tree.leaves.get(&parts).map(|leaf| json_type(&leaf.value)) };
        let value_type = json_type(&value);
        // A list may become a plain list setting again (and back); an object stays a section.
        if !structured(&value) && is_section && !(is_list && value.is_array()) {
            return Err(RpcError::invalid_params(format!("`{path}` is a section, not a setting")));
        }
        if let Some(stored) = stored_type
            && stored != value_type
        {
            return Err(RpcError::invalid_params(format!("`{path}` holds a {stored}; a {value_type} is rejected (delete the setting to change its type)")));
        }
        let now = now_ms();
        let description = description.or_else(|| tree.value(&parts).and_then(|(_, _, _, description)| description));
        let mut changes = Vec::new();
        // What is under the path now and is not in the new value goes.
        let wanted: BTreeSet<&Vec<String>> = flat.iter().map(|(path, _)| path).collect();
        let gone: Vec<Vec<String>> = tree.leaves.keys().filter(|key| under(key, &parts) && !wanted.contains(key)).cloned().collect();
        let kept: BTreeSet<&Vec<String>> = sections.iter().map(|(path, _)| path).collect();
        let gone_sections: Vec<Vec<String>> = tree.sections.keys().filter(|key| under(key, &parts) && !kept.contains(key)).cloned().collect();
        // Settings only in the old text were never nodes: their removal is not a change of the tree.
        for path in gone.iter().filter(|path| !tree.old_leaves.contains(*path)) {
            changes.push(json!({ "scope": scope, "path": path, "op": "delete", "by": by }));
        }
        self.remove(scope, &gone, &gone_sections).await?;
        let mut leaves = Vec::new();
        for (leaf_path, leaf_value) in &flat {
            let own = if *leaf_path == parts { description.clone() } else { tree.leaves.get(leaf_path).and_then(|leaf| leaf.description.clone()) };
            let unchanged = tree.leaves.get(leaf_path).is_some_and(|leaf| leaf.value == *leaf_value && leaf.description == own);
            // A setting only in the old text is written even when unchanged: the text lets go of it.
            if unchanged && !tree.old_leaves.contains(leaf_path) {
                continue;
            }
            leaves.push((leaf_path.clone(), Leaf { value: leaf_value.clone(), description: own, updated_ms: now, updated_by: by.to_owned() }));
            if !unchanged {
                changes.push(json!({ "scope": scope, "path": leaf_path, "op": "set", "value": leaf_value, "by": by }));
            }
        }
        let mut shaped = Vec::new();
        for (section, list) in &sections {
            let stored = tree.sections.get(section);
            let description = if *section == parts { description.clone() } else { stored.and_then(|stored| stored.description.clone()) };
            if tree.old_sections.contains(section) || stored.is_none_or(|stored| stored.list != *list || stored.description != description) {
                shaped.push((section.clone(), Section { description, list: *list }));
            }
        }
        self.write_batch(scope, &leaves, &shaped).await?;
        self.forget_old(scope, &parts).await?;
        drop(_write);
        self.record(changes).await;
        Ok(SettingValue { path: path.to_owned(), value, value_type: value_type.to_owned(), description, updated_ms: now, updated_by: by.to_owned() })
    }

    /// Every setting under `prefix` as nested JSON.
    pub async fn list(&self, scope: &str, prefix: &str) -> Result<Value, RpcError> {
        let parts = if prefix.is_empty() { Vec::new() } else { segments(prefix)? };
        let tree = self.read(scope_of(scope)).await?;
        if let Some(leaf) = tree.leaves.get(&parts) {
            return Ok(leaf.value.clone());
        }
        let mut root = Map::new();
        for (path, leaf) in &tree.leaves {
            if under(path, &parts) {
                insert_nested(&mut root, &path[parts.len()..], leaf.value.clone());
            }
        }
        Ok(tree.shaped(Value::Object(root), &parts))
    }

    /// Delete a setting, or a section with everything under it. Returns the deleted setting paths.
    pub async fn delete(&self, scope: &str, path: &str) -> Result<Vec<String>, RpcError> {
        let parts = segments(path)?;
        let scope = scope_of(scope);
        let _write = self.write.lock().await;
        let tree = self.load(scope).await?;
        let doomed: Vec<Vec<String>> = tree.leaves.keys().filter(|key| under(key, &parts)).cloned().collect();
        if doomed.is_empty() {
            return Err(RpcError::not_found(format!("no setting or section `{path}`")));
        }
        let sections: Vec<Vec<String>> = tree.sections.keys().filter(|key| under(key, &parts)).cloned().collect();
        self.remove(scope, &doomed, &sections).await?;
        self.forget_old(scope, &parts).await?;
        drop(_write);
        // One record for what only the old text held (a record moved elsewhere), one per node deleted.
        let mut changes: Vec<Value> = doomed.iter().filter(|path| !tree.old_leaves.contains(*path)).map(|path| json!({ "scope": scope, "path": path, "op": "delete", "by": "" })).collect();
        if doomed.iter().any(|path| tree.old_leaves.contains(path)) {
            changes.push(json!({ "scope": scope, "path": parts, "op": "delete", "by": "", "old": true }));
        }
        self.record(changes).await;
        Ok(doomed.iter().map(|path| dotted(path)).collect())
    }
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
}

fn insert_nested(node: &mut Map<String, Value>, parts: &[String], value: Value) {
    match parts {
        [] => {}
        [last] => {
            node.insert(last.clone(), value);
        }
        [head, rest @ ..] => {
            let entry = node.entry(head.clone()).or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Value::Object(child) = entry {
                insert_nested(child, rest, value);
            }
        }
    }
}

/// The settings and sections one entry of the old JSON text becomes (`{value, description,
/// updated_ms, updated_by}` at a dotted path). An entry that cannot be kept is logged and gives none.
fn old_entry(path: &[String], entry: &Value) -> (Vec<(Vec<String>, Leaf)>, Vec<(Vec<String>, Section)>) {
    let value = entry.get("value").cloned().unwrap_or(Value::Null);
    let updated_ms = entry.get("updated_ms").and_then(Value::as_u64).unwrap_or(0);
    let by = entry.get("updated_by").and_then(Value::as_str).unwrap_or("").to_owned();
    let description = entry.get("description").and_then(Value::as_str).map(str::to_owned);
    let (mut flat, mut sections) = (Vec::new(), Vec::new());
    if let Err(error) = flatten(path, &value, &mut flat, &mut sections) {
        tracing::warn!(path = %dotted(path), error = %error.message, "an old setting cannot be kept");
        return (Vec::new(), Vec::new());
    }
    let leaves = flat
        .into_iter()
        .map(|(leaf_path, leaf_value)| {
            let own = if leaf_path == path { description.clone() } else { None };
            (leaf_path, Leaf { value: leaf_value, description: own, updated_ms, updated_by: by.clone() })
        })
        .collect();
    let sections = sections
        .into_iter()
        .map(|(section_path, list)| {
            let own = if section_path == path { description.clone() } else { None };
            (section_path, Section { description: own, list })
        })
        .collect();
    (leaves, sections)
}
