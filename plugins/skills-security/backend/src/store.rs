//! The sentinel's calls to the memory plugin: the projects, each project's findings and their
//! judgments, and the sentinel's own records in the app's graph (docs/GRAPH-MODEL.md, "Security
//! sentinel"): `SecuritySentinel` (the feed cursor, when each sweep last ran, the digest), one
//! `SecurityFeed` per live source, one `SecurityWatch` per project (what the last sweep saw of
//! its tree) and the `SecurityMute` rules, all on the `Machine`.

use cc_protocol::contracts::memory::{
    GraphEdge, GraphLayer, GraphNode, GraphPutParams, GraphQueryParams, GraphQueryResult, GraphRef, GraphRetireParams, MachineInfo, ProjectInfo, ProjectListResult, SecurityFinding,
    SecurityFindingDetail, SecurityFindingsResult, SecuritySummary, SecurityTriage, methods,
};
use cc_protocol::contracts::security::{FeedState, MuteRule};
use cc_protocol::{Ctx, RpcError, parse_params};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// Findings written per call.
const PUT_CHUNK: usize = 200;

pub async fn get<T: DeserializeOwned>(ctx: &Ctx, method: &str, params: Value) -> Result<T, RpcError> {
    parse_params(ctx.call(method, params).await?)
}

/// The projects to watch: every registered one that is not a read-only reference and whose folder
/// is still there.
pub async fn projects(ctx: &Ctx) -> Result<Vec<ProjectInfo>, RpcError> {
    let list: ProjectListResult = get(ctx, methods::PROJECT_LIST, json!({})).await?;
    Ok(list.projects.into_iter().filter(|p| !p.reference && !p.root.is_empty() && std::path::Path::new(&p.root).is_dir()).collect())
}

pub async fn findings(ctx: &Ctx, project_id: &str, statuses: &[&str], triage: Option<&[&str]>) -> Result<Vec<SecurityFinding>, RpcError> {
    let mut params = json!({"project_id": project_id, "status": statuses});
    if let Some(triage) = triage {
        params["triage"] = json!(triage);
    }
    Ok(get::<SecurityFindingsResult>(ctx, methods::SECURITY_FINDINGS, params).await?.findings)
}

pub async fn finding(ctx: &Ctx, project_id: &str, id: &str) -> Result<SecurityFinding, RpcError> {
    Ok(get::<SecurityFindingDetail>(ctx, methods::SECURITY_FINDING, json!({"project_id": project_id, "id": id})).await?.finding)
}

pub async fn summary(ctx: &Ctx, project_id: &str) -> Result<SecuritySummary, RpcError> {
    get(ctx, methods::SECURITY_SUMMARY, json!({"project_id": project_id})).await
}

/// Put findings in chunks; returns how many the store kept.
pub async fn put_findings(ctx: &Ctx, project_id: &str, findings: &[Value]) -> Result<usize, RpcError> {
    let mut kept = 0;
    for chunk in findings.chunks(PUT_CHUNK) {
        let result: SecurityFindingsResult = get(ctx, methods::SECURITY_FINDINGS_PUT, json!({"project_id": project_id, "findings": chunk})).await?;
        kept += result.findings.len();
    }
    Ok(kept)
}

pub async fn judge(ctx: &Ctx, project_id: &str, items: Vec<SecurityTriage>) -> Result<Vec<SecurityFinding>, RpcError> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let result: SecurityFindingsResult = get(ctx, methods::SECURITY_TRIAGE_PUT, json!({"project_id": project_id, "items": items})).await?;
    Ok(result.findings)
}

/// A sweep finding the sweep no longer sees: closed by the host.
pub async fn close_swept(ctx: &Ctx, project_id: &str, id: &str) -> Result<(), RpcError> {
    ctx.call(methods::SECURITY_FINDING_UPDATE, json!({"project_id": project_id, "id": id, "status": "fixed", "by": "host", "note": "the security sweep no longer finds it"})).await?;
    Ok(())
}

/// The host's evidence re-check of dependency findings (a package no longer pinned is fixed).
pub async fn recheck(ctx: &Ctx, project_id: &str, ids: &[String]) -> Result<(), RpcError> {
    if ids.is_empty() {
        return Ok(());
    }
    ctx.call(methods::SECURITY_RECHECK, json!({"project_id": project_id, "ids": ids})).await?;
    Ok(())
}

// ---- the sentinel's records in the app's graph ------------------------------------------------------

pub async fn machine(ctx: &Ctx) -> Result<GraphRef, RpcError> {
    let info: MachineInfo = get(ctx, methods::GRAPH_MACHINE, json!({})).await?;
    Ok(GraphRef { label: "Machine".into(), key: info.key })
}

fn node(label: &str, key: &str, props: Map<String, Value>) -> GraphNode {
    GraphNode { label: label.into(), key: key.into(), props, ..Default::default() }
}

fn edge(from: GraphRef, rel: &str, to: GraphRef) -> GraphEdge {
    GraphEdge { from, rel: rel.into(), to, ..Default::default() }
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

async fn put(ctx: &Ctx, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> Result<(), RpcError> {
    let params = GraphPutParams { nodes, edges, layer: GraphLayer::Workspace, at_ms: None };
    ctx.call(methods::GRAPH_PUT, serde_json::to_value(&params)?).await?;
    Ok(())
}

async fn query(ctx: &Ctx, cypher: &str, params: Map<String, Value>) -> Result<GraphQueryResult, RpcError> {
    get(ctx, methods::GRAPH_QUERY, serde_json::to_value(GraphQueryParams { cypher: cypher.into(), params, limit: Some(5000) })?).await
}

/// The properties of the node a query returns in its first column, with its key (the second
/// column) under `key`.
fn props_of(row: &[Value]) -> Map<String, Value> {
    let mut props = row.first().and_then(|node| node.get("properties")).and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(key) = row.get(1).filter(|key| key.is_string()) {
        props.insert("key".into(), key.clone());
    }
    props
}

/// The sentinel's state kept across restarts.
#[derive(Debug, Clone, Default)]
pub struct Saved {
    pub cursor_ms: u64,
    pub last_delta_ms: u64,
    pub last_full_ms: u64,
    pub digest_ms: u64,
    pub routines_moved: bool,
    pub machine_told: Vec<String>,
    pub feeds: Vec<FeedState>,
    /// Project id -> the stamp of its tree at the last full sweep.
    pub stamps: HashMap<String, String>,
    /// Project id -> the posture gaps the last full sweep saw.
    pub posture: HashMap<String, Vec<String>>,
}

pub async fn load(ctx: &Ctx) -> Result<Saved, RpcError> {
    let mut saved = Saved::default();
    let number = |props: &Map<String, Value>, key: &str| props.get(key).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0);
    let strings = |props: &Map<String, Value>, key: &str| -> Vec<String> { props.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default() };
    let rows = query(ctx, "MATCH (s:SecuritySentinel) WHERE s.ended_ms IS NULL RETURN s, s.key LIMIT 1", Map::new()).await?;
    if let Some(row) = rows.rows.first() {
        let props = props_of(row);
        saved.cursor_ms = number(&props, "cursor_ms");
        saved.last_delta_ms = number(&props, "last_delta_ms");
        saved.last_full_ms = number(&props, "last_full_ms");
        saved.digest_ms = number(&props, "digest_ms");
        saved.routines_moved = props.get("routines_moved").and_then(Value::as_bool).unwrap_or(false);
        saved.machine_told = strings(&props, "machine_told");
    }
    let rows = query(ctx, "MATCH (f:SecurityFeed) WHERE f.ended_ms IS NULL RETURN f, f.key", Map::new()).await?;
    for row in &rows.rows {
        let props = props_of(row);
        saved.feeds.push(FeedState {
            name: props.get("key").and_then(Value::as_str).unwrap_or_default().to_owned(),
            checked_ms: Some(number(&props, "checked_ms")).filter(|ms| *ms > 0),
            ok: props.get("ok").and_then(Value::as_bool).unwrap_or(false),
            note: props.get("note").and_then(Value::as_str).unwrap_or_default().to_owned(),
        });
    }
    let rows = query(ctx, "MATCH (w:SecurityWatch) WHERE w.ended_ms IS NULL RETURN w, w.key", Map::new()).await?;
    for row in &rows.rows {
        let props = props_of(row);
        let Some(project) = props.get("key").and_then(Value::as_str).map(str::to_owned) else { continue };
        if let Some(stamp) = props.get("stamp").and_then(Value::as_str) {
            saved.stamps.insert(project.clone(), stamp.to_owned());
        }
        saved.posture.insert(project, strings(&props, "posture"));
    }
    Ok(saved)
}

/// Write the sentinel node and the feeds.
pub async fn save(ctx: &Ctx, saved: &Saved) -> Result<(), RpcError> {
    let machine = machine(ctx).await?;
    let mut nodes = vec![node(
        "SecuritySentinel",
        &machine.key,
        object(json!({
            "cursor_ms": saved.cursor_ms, "last_delta_ms": saved.last_delta_ms, "last_full_ms": saved.last_full_ms,
            "digest_ms": saved.digest_ms, "routines_moved": saved.routines_moved, "machine_told": saved.machine_told,
        })),
    )];
    let mut edges = vec![edge(GraphRef { label: "SecuritySentinel".into(), key: machine.key.clone() }, "ON", machine.clone())];
    for feed in &saved.feeds {
        nodes.push(node("SecurityFeed", &feed.name, object(json!({"checked_ms": feed.checked_ms, "ok": feed.ok, "note": feed.note}))));
        edges.push(edge(GraphRef { label: "SecurityFeed".into(), key: feed.name.clone() }, "ON", machine.clone()));
    }
    put(ctx, nodes, edges).await
}

/// What a full sweep saw of one project's tree.
pub async fn save_watch(ctx: &Ctx, project_id: &str, stamp: &str, packages: u32, posture: &[String], swept_ms: u64) -> Result<(), RpcError> {
    let props = object(json!({"stamp": stamp, "packages": packages, "posture": posture, "swept_ms": swept_ms}));
    let watch = GraphRef { label: "SecurityWatch".into(), key: project_id.into() };
    put(ctx, vec![node("SecurityWatch", project_id, props)], vec![edge(watch, "OF", GraphRef { label: "Project".into(), key: project_id.into() })]).await
}

// ---- mute rules -------------------------------------------------------------------------------------

const MUTE_FIELDS: &[&str] = &["kind", "project_id", "finding_id", "fleet_key", "ecosystem", "package", "advisory", "class", "path_prefix", "note", "label", "created_ms", "until_ms"];

pub async fn mutes(ctx: &Ctx) -> Result<Vec<MuteRule>, RpcError> {
    let rows = query(ctx, "MATCH (m:SecurityMute) WHERE m.ended_ms IS NULL RETURN m, m.key", Map::new()).await?;
    let mut out = Vec::new();
    for row in &rows.rows {
        let props = props_of(row);
        let mut rule = Map::new();
        rule.insert("key".into(), props.get("key").cloned().unwrap_or(Value::Null));
        for field in MUTE_FIELDS {
            if let Some(value) = props.get(*field).filter(|v| !v.is_null()) {
                let value = match value.as_f64() {
                    Some(f) if !value.is_i64() && !value.is_u64() && f.fract() == 0.0 => json!(f as u64),
                    _ => value.clone(),
                };
                rule.insert((*field).into(), value);
            }
        }
        if let Ok(rule) = serde_json::from_value::<MuteRule>(Value::Object(rule)) {
            out.push(rule);
        }
    }
    out.sort_by_key(|rule| std::cmp::Reverse(rule.created_ms));
    Ok(out)
}

pub async fn mute_put(ctx: &Ctx, rule: &MuteRule) -> Result<(), RpcError> {
    let machine = machine(ctx).await?;
    let mut props = object(serde_json::to_value(rule)?);
    props.remove("key");
    props.retain(|_, value| !value.is_null());
    put(ctx, vec![node("SecurityMute", &rule.key, props)], vec![edge(GraphRef { label: "SecurityMute".into(), key: rule.key.clone() }, "ON", machine)]).await
}

pub async fn mute_retire(ctx: &Ctx, key: &str) -> Result<(), RpcError> {
    let params = GraphRetireParams { refs: vec![GraphRef { label: "SecurityMute".into(), key: key.into() }], reason: Some("removed by the person".into()), ..Default::default() };
    ctx.call(methods::GRAPH_RETIRE, serde_json::to_value(&params)?).await?;
    Ok(())
}
