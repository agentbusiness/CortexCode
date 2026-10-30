//! What the mesh keeps, in the app's graph (`memory@1`, `docs/GRAPH-MODEL.md`, Mesh). Nothing of it
//! is a file any more; the node's key is in the Keychain (`crate::identity`).
//!
//! - **The membership log**: stream `mesh.members`, every entry of the signed chain in order. A
//!   record `{"reset": true}` starts a new chain (the node left its network, or adopted another
//!   one); reading the stream from the start and replaying gives the log back. Kept 3650 days.
//! - **The membership as a graph**, built from the log: `(:Member {key: node id, name*, added_ms,
//!   added_by, software})-[:IN]->(:Mesh {key: hash of the first entry, founder, founded_ms})
//!   -[:ON]->(:Machine)`, in the WORKSPACE layer. A revoked member is retired (`revoked`); a mesh
//!   this node is no longer in is retired (`left`), with its members.
//! - **Served calls**: stream `mesh.served`, one record per call served to another app:
//!   `{at, node, method, ok, ms}`, kept 30 days.
//! - **The person's choices**: `memory.settings` (global scope) `mesh.name` (string), `mesh.lend`
//!   (boolean) and `mesh.shares` (an object: service -> `{local, network}`).
//!
//! **Start order.** The memory plugin may answer later than the mesh starts. Until it does, the
//! mesh runs on what it holds in memory: the network endpoint starts at once, so apps on this
//! machine (found through `crate::local`, which needs no log) link and lend as before; members of
//! the network are not let in, and nothing that changes the membership is done, until the log is
//! read (`Mesh::wait_loaded`, bounded; the methods then answer `unavailable`). The read is tried
//! again every second for as long as it takes. Writes go through one ordered writer that keeps
//! a write the store did not take and tries it again, so nothing written before the store answers
//! is lost while the process runs.
//!
//! **Files of earlier versions** (`<home>/mesh/members.jsonl`, `served.jsonl`, `shares.json`,
//! `name`, `no-lend`) are read once when the store first answers, written to it, confirmed, and
//! deleted.

use crate::members::{Entry, Log};
use cc_protocol::contracts::memory::methods;
use cc_protocol::{BoxFuture, RpcError, codes};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// The membership log's entries, in order.
pub const MEMBERS_TOPIC: &str = "mesh.members";
/// Every call served to another app.
pub const SERVED_TOPIC: &str = "mesh.served";
/// Settings paths (global scope).
pub const NAME_SETTING: &str = "mesh.name";
pub const LEND_SETTING: &str = "mesh.lend";
pub const SHARES_SETTING: &str = "mesh.shares";

/// How long served calls are kept (days).
pub const SERVED_DAYS: u32 = 30;
const SERVED_KEEP_MS: u64 = SERVED_DAYS as u64 * 24 * 60 * 60 * 1000;
/// The membership stream is kept as long as the store keeps anything.
const MEMBERS_DAYS: u32 = 3650;
/// Between two attempts while the store does not answer.
const RETRY: Duration = Duration::from_secs(1);
/// Records per stream read.
const PAGE: u32 = 5000;

/// A call into the app's store: `ctx.call` in the plugin, `Memory::dispatch` in a test harness.
pub type Caller = Arc<dyn Fn(String, Value) -> BoxFuture<'static, Result<Value, RpcError>> + Send + Sync>;

pub fn caller_of(ctx: cc_protocol::Ctx) -> Caller {
    Arc::new(move |method: String, params: Value| {
        let ctx = ctx.clone();
        Box::pin(async move { ctx.call(&method, params).await })
    })
}

/// The store has not answered (yet): try again.
pub fn transient(error: &RpcError) -> bool {
    matches!(error.code.as_str(), codes::UNAVAILABLE | codes::CONNECTION_CLOSED | codes::DRAINING | codes::TIMEOUT)
}

/// Call until the store answers (a transient error is waited out); other errors come back.
async fn call_patiently(call: &Caller, method: &str, params: Value) -> Result<Value, RpcError> {
    let mut waited = 0u32;
    loop {
        match call(method.to_owned(), params.clone()).await {
            Err(error) if transient(&error) => {
                waited += 1;
                if waited == 10 {
                    tracing::info!(method, error = %error.message, "the memory plugin does not answer yet; the mesh keeps waiting for it");
                }
                tokio::time::sleep(RETRY).await;
            }
            other => return other,
        }
    }
}

/// What the store held when it first answered.
#[derive(Default)]
pub struct Loaded {
    pub machine: String,
    pub name: Option<String>,
    pub lend: bool,
    pub shares: HashMap<String, cc_protocol::contracts::service::Allowed>,
    /// The log as the stream replays (before validation: what the stream holds).
    pub entries: Vec<Entry>,
}

fn mesh_dir(home: &Path) -> PathBuf {
    home.join("mesh")
}

/// Read everything at start, moving the files of earlier versions in once. Waits for the store
/// for as long as it takes; an error is a store that refused (not one that is down).
pub async fn load(call: &Caller, home: &Path) -> Result<Loaded, RpcError> {
    let machine = call_patiently(call, methods::GRAPH_MACHINE, json!({})).await?;
    let machine = machine.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
    let dir = mesh_dir(home);

    // Settings: the store's value, else the old file's (written, confirmed, then the file goes).
    let name = match get_setting(call, NAME_SETTING).await? {
        Some(value) => value.as_str().map(str::to_owned),
        None => {
            let file = dir.join("name");
            let old = std::fs::read_to_string(&file).ok().map(|text| text.trim().to_owned()).filter(|name| !name.is_empty());
            if let Some(name) = &old {
                set_confirmed(call, NAME_SETTING, json!(name)).await?;
            }
            remove(&file);
            old
        }
    };
    let lend = match get_setting(call, LEND_SETTING).await? {
        Some(value) => value.as_bool().unwrap_or(true),
        None => {
            let file = dir.join("no-lend");
            let lend = !file.exists();
            if !lend {
                set_confirmed(call, LEND_SETTING, json!(false)).await?;
            }
            remove(&file);
            lend
        }
    };
    let shares = match get_setting(call, SHARES_SETTING).await? {
        Some(value) => serde_json::from_value(value).unwrap_or_default(),
        None => {
            let file = dir.join("shares.json");
            let old: HashMap<String, cc_protocol::contracts::service::Allowed> =
                std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
            if !old.is_empty() {
                set_confirmed(call, SHARES_SETTING, serde_json::to_value(&old).unwrap_or(Value::Null)).await?;
            }
            remove(&file);
            old
        }
    };

    // The membership log: the stream, else the old file's chain (appended, read back, then the
    // file goes).
    let mut entries = read_log(call).await?;
    let file = crate::members::log_path(home);
    if file.exists() {
        if entries.is_empty() {
            let old = Log::load(&file).entries;
            if !old.is_empty() {
                append(call, MEMBERS_TOPIC, old.iter().map(|entry| serde_json::to_value(entry).unwrap_or(Value::Null)).collect(), MEMBERS_DAYS).await?;
                entries = read_log(call).await?;
                if entries != old {
                    return Err(RpcError::internal("the membership log read back from the store is not the one written; members.jsonl was kept"));
                }
                tracing::info!(entries = old.len(), "the membership log moved into the store");
            }
        }
        remove(&file);
    }

    // Served calls of the last 30 days, unless the stream has them already.
    let file = dir.join("served.jsonl");
    if file.exists() {
        let horizon = cc_protocol::contracts::now_ms().saturating_sub(SERVED_KEEP_MS);
        let old: Vec<Value> = std::fs::read_to_string(&file)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["at"].as_u64().unwrap_or(0) >= horizon)
            .collect();
        let newest_kept = last_record(call, SERVED_TOPIC).await?.and_then(|value| value["at"].as_u64()).unwrap_or(0);
        let missing: Vec<Value> = old.into_iter().filter(|value| value["at"].as_u64().unwrap_or(0) > newest_kept).collect();
        let count = missing.len();
        for chunk in missing.chunks(PAGE as usize) {
            append(call, SERVED_TOPIC, chunk.to_vec(), SERVED_DAYS).await?;
        }
        if count > 0 {
            tracing::info!(records = count, "the record of served calls moved into the store");
        }
        remove(&file);
    }
    // The folder of earlier versions goes once it is empty.
    let _ = std::fs::remove_dir(&dir);

    Ok(Loaded { machine, name, lend, shares, entries })
}

fn remove(file: &Path) {
    match std::fs::remove_file(file) {
        Ok(()) => tracing::info!(file = %file.display(), "a file of an earlier version was moved into the store and removed"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(file = %file.display(), %error, "cannot remove a file already moved into the store"),
    }
}

async fn get_setting(call: &Caller, path: &str) -> Result<Option<Value>, RpcError> {
    match call_patiently(call, methods::SETTINGS_GET, json!({ "path": path })).await {
        Ok(setting) => Ok(setting.get("value").cloned()),
        Err(error) if error.code == codes::NOT_FOUND => Ok(None),
        Err(error) => Err(error),
    }
}

async fn set_confirmed(call: &Caller, path: &str, value: Value) -> Result<(), RpcError> {
    call_patiently(call, methods::SETTINGS_SET, json!({ "path": path, "value": value, "by": "mesh" })).await?;
    let back = get_setting(call, path).await?;
    if back.as_ref() != Some(&value) {
        return Err(RpcError::internal(format!("{path} read back from the store is not what was written; the old file was kept")));
    }
    Ok(())
}

async fn append(call: &Caller, topic: &str, records: Vec<Value>, days: u32) -> Result<(), RpcError> {
    if records.is_empty() {
        return Ok(());
    }
    call_patiently(call, methods::STREAM_APPEND, json!({ "topic": topic, "records": records, "retention_days": days })).await.map(|_| ())
}

/// Every record of a topic, oldest first, at most `cap`.
async fn read_all(call: &Caller, topic: &str, since_ms: Option<u64>, cap: usize) -> Result<Vec<Value>, RpcError> {
    let mut out = Vec::new();
    let mut from: Option<u64> = None;
    loop {
        let mut params = json!({ "topic": topic, "limit": PAGE });
        if let Some(from) = from {
            params["from_offset"] = json!(from);
        }
        if let Some(since) = since_ms {
            params["since_ms"] = json!(since);
        }
        let page = match call_patiently(call, methods::STREAM_READ, params).await {
            Ok(page) => page,
            // A topic nobody wrote yet.
            Err(error) if error.code == codes::NOT_FOUND => return Ok(out),
            Err(error) => return Err(error),
        };
        let records = page.get("records").and_then(Value::as_array).cloned().unwrap_or_default();
        let empty = records.is_empty();
        out.extend(records.into_iter().filter_map(|record| record.get("value").cloned()));
        let end = page.get("end").and_then(Value::as_bool).unwrap_or(true);
        if end || empty || out.len() >= cap {
            return Ok(out);
        }
        from = page.get("next_offset").and_then(Value::as_u64);
    }
}

/// The last record of a topic, if any.
async fn last_record(call: &Caller, topic: &str) -> Result<Option<Value>, RpcError> {
    let topics = call_patiently(call, methods::STREAM_TOPICS, json!({ "prefix": topic })).await?;
    let next = topics
        .get("topics")
        .and_then(Value::as_array)
        .and_then(|list| list.iter().find(|t| t["name"] == topic))
        .and_then(|t| t["next_offset"].as_u64())
        .unwrap_or(0);
    if next == 0 {
        return Ok(None);
    }
    let page = call_patiently(call, methods::STREAM_READ, json!({ "topic": topic, "from_offset": next - 1, "limit": 1 })).await?;
    Ok(page.get("records").and_then(Value::as_array).and_then(|records| records.first()).and_then(|record| record.get("value").cloned()))
}

/// The membership log as the stream replays it.
pub async fn read_log(call: &Caller) -> Result<Vec<Entry>, RpcError> {
    let mut entries = Vec::new();
    for value in read_all(call, MEMBERS_TOPIC, None, usize::MAX).await? {
        if value.get("reset").and_then(Value::as_bool) == Some(true) {
            entries.clear();
        } else if let Ok(entry) = serde_json::from_value::<Entry>(value) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Served calls since `since_ms` (by the time they were served), oldest first.
pub async fn read_served(call: &Caller, since_ms: u64) -> Result<Vec<Value>, RpcError> {
    let records = read_all(call, SERVED_TOPIC, Some(since_ms), 200_000).await?;
    Ok(records.into_iter().filter(|value| value["at"].as_u64().unwrap_or(0) >= since_ms).collect())
}

// --- the writer --------------------------------------------------------------------------------

pub enum Write {
    /// One served call.
    Served(Value),
    /// The log as it stands now.
    Log(Vec<Entry>),
    /// A member's software, from its card (written once per value).
    Software(String, String),
    /// A setting's new value.
    Setting(&'static str, Value),
}

/// The ordered writer, once the store has been read: `persisted` is what the membership stream
/// holds now (as [`Loaded::entries`]); `rx` has queued what changed meanwhile. Each write is tried
/// until the store takes it; a write the store refuses is logged and dropped (the next log write
/// appends what is missing).
pub async fn run_writer(call: Caller, machine: String, persisted: Vec<Entry>, mut rx: mpsc::UnboundedReceiver<Write>) {
    {
        let mut writer = Writer { call, machine, persisted, software: std::collections::HashSet::new() };
        writer.project_if_missing().await;
        let mut carried: Option<Write> = None;
        loop {
            let Some(write) = (match carried.take() { Some(write) => Some(write), None => rx.recv().await }) else { return };
            match write {
                Write::Served(first) => {
                    // Served calls queued together go in one append.
                    let mut batch = vec![first];
                    while let Ok(next) = rx.try_recv() {
                        match next {
                            Write::Served(value) if batch.len() < PAGE as usize => batch.push(value),
                            other => {
                                carried = Some(other);
                                break;
                            }
                        }
                    }
                    if let Err(error) = append(&writer.call, SERVED_TOPIC, batch, SERVED_DAYS).await {
                        tracing::warn!(error = %error.message, "served calls not recorded");
                    }
                }
                Write::Log(mut latest) => {
                    // Only the latest of several queued snapshots matters.
                    while let Ok(next) = rx.try_recv() {
                        match next {
                            Write::Log(newer) => latest = newer,
                            other => {
                                carried = Some(other);
                                break;
                            }
                        }
                    }
                    writer.log(latest).await;
                }
                Write::Software(node, software) => {
                    if writer.software.contains(&(node.clone(), software.clone())) {
                        continue;
                    }
                    match writer.software(&node, &software).await {
                        Ok(true) => drop(writer.software.insert((node, software))),
                        Ok(false) => {}
                        Err(error) => tracing::debug!(error = %error.message, "a member's software not recorded"),
                    }
                }
                Write::Setting(path, value) => {
                    if let Err(error) = call_patiently(&writer.call, methods::SETTINGS_SET, json!({ "path": path, "value": value, "by": "mesh" })).await {
                        tracing::warn!(path, error = %error.message, "a mesh setting was not stored");
                    }
                }
            }
        }
    }
}

struct Writer {
    call: Caller,
    machine: String,
    /// What the membership stream holds, as it replays.
    persisted: Vec<Entry>,
    /// Members' software already written for this mesh.
    software: std::collections::HashSet<(String, String)>,
}

fn node(label: &str, key: &str) -> Value {
    json!({ "label": label, "key": key })
}

impl Writer {
    /// The log as it stands: the entries the stream lacks are appended (a different chain is a
    /// reset, then the whole of it), and the graph follows.
    async fn log(&mut self, latest: Vec<Entry>) {
        let extends = latest.len() >= self.persisted.len() && latest[..self.persisted.len()] == self.persisted[..];
        if extends && latest.len() == self.persisted.len() {
            return;
        }
        let mut records = Vec::new();
        let (old_mesh, new_from) = if extends {
            (None, self.persisted.len())
        } else {
            records.push(json!({ "reset": true }));
            (self.persisted.first().map(Entry::hash), 0)
        };
        records.extend(latest[new_from..].iter().map(|entry| serde_json::to_value(entry).unwrap_or(Value::Null)));
        if let Err(error) = append(&self.call, MEMBERS_TOPIC, records, MEMBERS_DAYS).await {
            tracing::warn!(error = %error.message, "the membership log was not stored; it is written again with the next change");
            return;
        }
        let old_members: Vec<String> = Log { entries: self.persisted.clone() }.members().into_keys().collect();
        self.persisted = latest.clone();
        if let Some(old) = old_mesh {
            self.software.clear();
            let mut refs = vec![node("Mesh", &old)];
            let still: BTreeMap<String, _> = Log { entries: latest.clone() }.members();
            refs.extend(old_members.iter().filter(|member| !still.contains_key(*member)).map(|member| node("Member", member)));
            self.retire(refs, "left", None).await;
        }
        if let Err(error) = self.project(&latest, new_from).await {
            tracing::warn!(error = %error.message, "the membership graph was not brought up to date");
        }
    }

    /// The graph of the membership from entry `from` on, each change at its own time.
    async fn project(&self, entries: &[Entry], from: usize) -> Result<(), RpcError> {
        let Some(first) = entries.first() else { return Ok(()) };
        let mesh = first.hash();
        let now = cc_protocol::contracts::now_ms();
        let mesh_node = json!({ "label": "Mesh", "key": mesh, "props": { "founder": first.name, "founded_ms": first.ts_ms } });
        let on = json!({ "from": node("Mesh", &mesh), "rel": "ON", "to": node("Machine", &self.machine) });
        for entry in &entries[from..] {
            let at = entry.ts_ms.min(now);
            match entry.kind.as_str() {
                "add" => {
                    let member = json!({ "label": "Member", "key": entry.node, "props": { "added_ms": entry.ts_ms, "added_by": entry.by }, "temporal": { "name": entry.name } });
                    let edge = json!({ "from": node("Member", &entry.node), "rel": "IN", "to": node("Mesh", &mesh) });
                    self.put(vec![mesh_node.clone(), member], vec![on.clone(), edge], at).await?;
                }
                "rename" => {
                    let member = json!({ "label": "Member", "key": entry.node, "temporal": { "name": entry.name } });
                    let edge = json!({ "from": node("Member", &entry.node), "rel": "IN", "to": node("Mesh", &mesh) });
                    self.put(vec![mesh_node.clone(), member], vec![on.clone(), edge], at).await?;
                }
                "revoke" => self.retire(vec![node("Member", &entry.node)], "revoked", Some(at)).await,
                _ => {}
            }
        }
        Ok(())
    }

    /// At start: the graph is built from the whole log when it has no node for this mesh yet
    /// (the first start with the store, or a write that failed).
    async fn project_if_missing(&self) {
        let Some(first) = self.persisted.first() else { return };
        let found = call_patiently(
            &self.call,
            methods::GRAPH_QUERY,
            json!({ "cypher": "MATCH (x:Mesh {key: $mesh})-[:ON]->(:Machine {key: $machine}) RETURN count(x)", "params": { "mesh": first.hash(), "machine": self.machine } }),
        )
        .await;
        let present = found.as_ref().ok().and_then(|v| v["rows"][0][0].as_u64()).unwrap_or(0) > 0;
        if !present && let Err(error) = self.project(&self.persisted, 0).await {
            tracing::warn!(error = %error.message, "the membership graph was not built");
        }
    }

    /// Whether it was written (a node that is not a member of the log is not).
    async fn software(&self, member: &str, software: &str) -> Result<bool, RpcError> {
        let Some(first) = self.persisted.first() else { return Ok(false) };
        if !(Log { entries: self.persisted.clone() }).is_member(member) {
            return Ok(false);
        }
        let nodes = vec![json!({ "label": "Member", "key": member, "props": { "software": software } })];
        let edges = vec![json!({ "from": node("Member", member), "rel": "IN", "to": node("Mesh", &first.hash()) })];
        self.put(nodes, edges, cc_protocol::contracts::now_ms()).await.map(|()| true)
    }

    async fn put(&self, nodes: Vec<Value>, edges: Vec<Value>, at_ms: u64) -> Result<(), RpcError> {
        call_patiently(&self.call, methods::GRAPH_PUT, json!({ "nodes": nodes, "edges": edges, "layer": "workspace", "at_ms": at_ms })).await.map(|_| ())
    }

    async fn retire(&self, refs: Vec<Value>, reason: &str, at_ms: Option<u64>) {
        let mut params = json!({ "refs": refs, "reason": reason });
        if let Some(at) = at_ms {
            params["at_ms"] = json!(at);
        }
        if let Err(error) = call_patiently(&self.call, methods::GRAPH_RETIRE, params).await {
            tracing::warn!(error = %error.message, "members not marked as gone in the graph");
        }
    }
}
