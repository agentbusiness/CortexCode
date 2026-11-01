//! Which project roots were seeded from which image, kept in the app's graph (`memory@1`,
//! `docs/GRAPH-MODEL.md`):
//! `(:ContainerSeed {key: root, root, image, image_id, device, inode})-[:ON]->(:Machine)`, and
//! `-[:OF]->(:Workspace {key: root})` when the tools plugin knows the root as a workspace.
//!
//! Earlier versions kept the same facts as `<data>/seeds/<root hash>.json`; those files are moved
//! into the graph once ([`migrate`]) and deleted.

use cc_protocol::contracts::memory::{
    GraphEdge, GraphLayer, GraphNode, GraphPutParams, GraphQueryParams, GraphQueryResult, GraphRef,
    MachineInfo, methods,
};
use cc_protocol::{BoxFuture, RpcError, codes};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;

/// A call to another plugin through the frame (`memory.*` here).
pub type Caller = Arc<dyn Fn(&'static str, Value) -> BoxFuture<'static, Result<Value, RpcError>> + Send + Sync>;

const LABEL: &str = "ContainerSeed";

/// A root seeded from an image: the image's id and the published folder's device and inode (absent
/// in markers from versions that did not record them).
#[derive(Debug, Clone, PartialEq)]
pub struct Seed {
    pub root: String,
    pub image: String,
    pub image_id: String,
    pub device: Option<u64>,
    pub inode: Option<u64>,
}

impl Seed {
    fn from_marker(value: &Value) -> Option<Seed> {
        Some(Seed {
            root: value.get("root")?.as_str()?.to_owned(),
            image: value.get("image").and_then(Value::as_str).unwrap_or_default().to_owned(),
            image_id: value.get("image_id")?.as_str()?.to_owned(),
            device: value.get("device").and_then(Value::as_u64),
            inode: value.get("inode").and_then(Value::as_u64),
        })
    }
}

fn gref(label: &str, key: &str) -> GraphRef {
    GraphRef { label: label.to_owned(), key: key.to_owned() }
}

fn edge(from: &GraphRef, rel: &str, to: GraphRef) -> GraphEdge {
    GraphEdge { from: from.clone(), rel: rel.to_owned(), to, props: Map::new(), temporal: Map::new() }
}

/// The memory plugin does not serve (not connected yet, starting, swapping, or gone).
pub fn away(error: &RpcError) -> bool {
    matches!(
        error.code.as_str(),
        codes::UNAVAILABLE | codes::UNKNOWN_METHOD | codes::CONNECTION_CLOSED | codes::DRAINING | codes::TIMEOUT
    )
}

/// The memory plugin's refusal of an edge whose end `Workspace:<root>` is not stored (its
/// `graphapi.rs`: "names L:k, which is neither in this batch nor stored").
fn workspace_unstored(error: &RpcError, root: &str) -> bool {
    error.code == codes::INVALID_PARAMS
        && error.message.contains(&format!("names Workspace:{root}, which is neither in this batch nor stored"))
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::internal(error.to_string())
}

/// The seed recorded for `root`, if any.
pub async fn read(call: &Caller, root: &str) -> Result<Option<Seed>, RpcError> {
    let mut params = Map::new();
    params.insert("root".to_owned(), json!(root));
    let request = GraphQueryParams {
        cypher: "MATCH (s:ContainerSeed) WHERE s.key = $root AND s.ended_ms IS NULL \
                 RETURN s.image, s.image_id, s.device, s.inode LIMIT 1"
            .to_owned(),
        params,
        limit: Some(1),
    };
    let answer = call(methods::GRAPH_QUERY, serde_json::to_value(request).map_err(internal)?).await?;
    let result: GraphQueryResult = serde_json::from_value(answer).map_err(internal)?;
    let Some(row) = result.rows.into_iter().next() else { return Ok(None) };
    let text = |i: usize| row.get(i).and_then(Value::as_str).unwrap_or_default().to_owned();
    let number = |i: usize| row.get(i).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)));
    let image_id = text(1);
    if image_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(Seed { root: root.to_owned(), image: text(0), image_id, device: number(2), inode: number(3) }))
}

/// Record a seed (and wait until the graph took it): linked to this Mac, and to the root's
/// workspace when the graph has one.
pub async fn write(call: &Caller, seed: &Seed) -> Result<(), RpcError> {
    let answer = call(methods::GRAPH_MACHINE, json!({})).await?;
    let machine: MachineInfo = serde_json::from_value(answer).map_err(internal)?;
    let mut props = Map::new();
    props.insert("root".to_owned(), json!(seed.root));
    props.insert("image".to_owned(), json!(seed.image));
    props.insert("image_id".to_owned(), json!(seed.image_id));
    if let Some(device) = seed.device {
        props.insert("device".to_owned(), json!(device));
    }
    if let Some(inode) = seed.inode {
        props.insert("inode".to_owned(), json!(inode));
    }
    let me = gref(LABEL, &seed.root);
    let node = GraphNode { label: LABEL.to_owned(), key: seed.root.clone(), props, temporal: Map::new() };
    let on = edge(&me, "ON", gref("Machine", &machine.key));
    let of = edge(&me, "OF", gref("Workspace", &seed.root));
    let with_workspace = GraphPutParams {
        nodes: vec![node.clone()],
        edges: vec![on.clone(), of],
        layer: GraphLayer::Workspace,
        at_ms: None,
    };
    match call(methods::GRAPH_PUT, serde_json::to_value(with_workspace).map_err(internal)?).await {
        Ok(_) => return Ok(()),
        Err(error) if workspace_unstored(&error, &seed.root) => {}
        Err(error) => return Err(error),
    }
    // The root is not (yet) a workspace the graph knows: the seed is kept on this Mac alone.
    let alone = GraphPutParams { nodes: vec![node], edges: vec![on], layer: GraphLayer::Workspace, at_ms: None };
    call(methods::GRAPH_PUT, serde_json::to_value(alone).map_err(internal)?).await?;
    Ok(())
}

/// The marker file an earlier version wrote for `root`, moved into the graph and deleted.
/// `Ok(None)` when there is none (or it cannot be read).
pub async fn migrate_one(call: &Caller, marker: &Path) -> Result<Option<Seed>, RpcError> {
    let Ok(text) = std::fs::read_to_string(marker) else { return Ok(None) };
    let Some(seed) = serde_json::from_str::<Value>(&text).ok().as_ref().and_then(Seed::from_marker) else {
        let _ = std::fs::remove_file(marker);
        return Ok(None);
    };
    write(call, &seed).await?;
    let _ = std::fs::remove_file(marker);
    Ok(Some(seed))
}

/// Move every marker file under `dir` (`<data>/seeds`) into the graph, once; a marker the graph
/// does not take yet stays for the next start. The folder goes when it is empty.
pub async fn migrate(call: &Caller, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && name.ends_with(".tmp") {
            // A marker an interrupted seed never finished.
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if !name.ends_with(".json") {
            continue;
        }
        if let Err(error) = migrate_one(call, &path).await {
            tracing::warn!(marker = %path.display(), error = %error.message, "a seed marker was not moved into the graph yet");
        }
    }
    let _ = std::fs::remove_dir(dir);
}
