//! Security findings in the WORKSPACE layer: the security plugin's audit runs and the findings they
//! report, with each finding's lifecycle (status, who moved it, its history).
//!
//! ```text
//! (:Security:Finding {id, project, ...SecurityFinding's scalar fields})
//! (:Security:Audit {id, project, ...SecurityAudit fields})
//! (f)-[:HAS_STEP]->(:SecurityTraceStep {id, finding_id, position, kind, file, line, what})
//! (f)-[:QUICK_FIX]->(:SecurityQuickFix {id, finding_id, kind, ecosystem, package, from, to, lockfile, commands})
//! (f)-[:CHANGED]->(:SecurityStatusChange {id, finding_id, position, status, note, by, ms, session_id, commit})
//! (f)-[:FOUND_BY]->(a:Audit)                     every audit that reported it
//! (f)-[:AT {step, kind, what, line}]->(c:Code)   each trace step's innermost symbol, else its file
//! (f)-[:IN_PACKAGE]->(d:Code:Dependency)         the package a quick fix upgrades
//! (f)-[:FIXED_IN]->(s:Session)                   the session that fixes or fixed it
//! stream `security.status`                     every status move, in order
//! ```
//!
//! A finding's trace, quick fix and lifecycle are nodes of their own, linked to it; the code the
//! trace points at is reachable by the `AT` edges. A finding is identified by its fingerprint, so
//! an audit that reports it again updates the node instead of adding one. Findings an earlier
//! version wrote with these as JSON text are read from it and rewritten as nodes at their next
//! write.

use crate::db::{Graph, layer};
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{
    SecurityAudit, SecurityFinding, SecurityFindingDetail, SecurityFindingUpdateParams, SecurityFindingsParams, SecurityQuickFix, SecurityStatusChange, SecuritySummary,
    SecurityTraceStep,
};
use std::collections::HashMap;
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// The statuses `memory.security.findings` lists when the caller names none: the work still open.
const ACTIVE: [&str; 3] = ["open", "fixing", "accepted"];

/// Finding properties, in the order `finding_from` reads them (the JSON field names of
/// [`SecurityFinding`], plus `history`).
const FINDING_FIELDS: [&str; 37] = [
    "id",
    "class",
    "verdict",
    "title",
    "principal",
    "control",
    "impact",
    "result",
    "evidence_how",
    "evidence_observed",
    "severity_likelihood",
    "severity_impact",
    "severity_overall",
    "fix_where",
    "fix_change",
    "fix_test",
    "missing_fact",
    "trace",
    "quick_fix",
    "priority",
    "priority_reason",
    "status",
    "status_note",
    "status_by",
    "status_ms",
    "first_seen_ms",
    "last_seen_ms",
    "seen_in",
    "audit_id",
    "session_id",
    "fixed_commit",
    "sink_path",
    "sink_line",
    "history",
    "project",
    "name",
    "updated_ms",
];

/// The stream every status move is appended to.
const STATUS_TOPIC: &str = "security.status";

/// Audit properties, in the order `audit_from` reads them (the JSON field names of [`SecurityAudit`]).
const AUDIT_FIELDS: [&str; 11] = ["id", "started_ms", "finished_ms", "units", "lines_judged", "findings", "leads", "hardening", "scope", "skill_version", "report_path"];

/// The security store of one IronGraph project (a repository).
pub struct SecurityStore {
    pub graph: Arc<Graph>,
    pub ig_id: String,
    /// Frame project id.
    pub scope: String,
}

fn columns(var: &str, fields: &[&str]) -> String {
    fields.iter().map(|field| format!("{var}.{field}")).collect::<Vec<_>>().join(", ")
}

/// A stored number comes back as it was written or as a float; serde wants the integer it was.
fn whole(value: Value) -> Value {
    match value.as_f64() {
        Some(f) if !value.is_i64() && !value.is_u64() && f.fract() == 0.0 => json!(f as i64),
        _ => value,
    }
}

/// A row as a JSON object keyed by its field names, without the properties the node lacks.
fn object(row: &[Value], fields: &[&str]) -> Map<String, Value> {
    fields.iter().zip(row).filter(|(_, value)| !value.is_null()).map(|(field, value)| ((*field).to_owned(), whole(value.clone()))).collect()
}

/// The JSON text an earlier version kept in a property, parsed into its type.
fn from_text<T: serde::de::DeserializeOwned>(object: &Map<String, Value>, field: &str) -> Option<T> {
    object.get(field).and_then(|value| value.as_str()).and_then(|text| serde_json::from_str(text).ok())
}

/// A finding's parts kept as nodes of their own.
#[derive(Default)]
struct Parts {
    trace: Vec<SecurityTraceStep>,
    quick_fix: Option<SecurityQuickFix>,
    history: Vec<SecurityStatusChange>,
}

fn finding_from(row: &[Value], parts: Option<&Parts>) -> Result<SecurityFinding, RpcError> {
    let mut object = object(row, &FINDING_FIELDS);
    let empty = Parts::default();
    let parts = parts.unwrap_or(&empty);
    let trace = if parts.trace.is_empty() { from_text::<Vec<SecurityTraceStep>>(&object, "trace").unwrap_or_default() } else { parts.trace.clone() };
    let quick_fix = parts.quick_fix.clone().or_else(|| from_text::<Option<SecurityQuickFix>>(&object, "quick_fix").flatten());
    object.insert("trace".into(), json!(trace));
    object.insert("quick_fix".into(), json!(quick_fix));
    object.remove("history");
    serde_json::from_value(Value::Object(object)).map_err(|error| RpcError::internal(format!("unreadable finding record: {error}")))
}

fn history_from(row: &[Value], parts: Option<&Parts>) -> Vec<SecurityStatusChange> {
    match parts {
        Some(parts) if !parts.history.is_empty() => parts.history.clone(),
        _ => from_text(&object(row, &FINDING_FIELDS), "history").unwrap_or_default(),
    }
}

fn props_of(node: &Value) -> Map<String, Value> {
    node.get("properties").and_then(Value::as_object).cloned().unwrap_or_default().into_iter().map(|(k, v)| (k, whole(v))).collect()
}

fn audit_from(row: &[Value]) -> Result<SecurityAudit, RpcError> {
    serde_json::from_value(Value::Object(object(row, &AUDIT_FIELDS))).map_err(|error| RpcError::internal(format!("unreadable audit record: {error}")))
}

/// The moves the lifecycle allows from a status (see [`SecurityFinding`]); `fixed` from `open`
/// is the re-check's alone and is judged by the caller.
fn allowed_moves(status: &str) -> &'static [&'static str] {
    match status {
        "open" => &["accepted", "fixing", "gone", "fixed"],
        "fixing" => &["fixed", "open"],
        "accepted" => &["open", "gone"],
        "fixed" => &["verified", "open"],
        "verified" | "gone" => &["open"],
        _ => &[],
    }
}

fn severity_key(finding: &SecurityFinding) -> String {
    match finding.verdict.as_str() {
        "needs_validation" => "lead".into(),
        "hardening" => "hardening".into(),
        _ => finding.severity_overall.clone().filter(|severity| !severity.is_empty()).unwrap_or_else(|| "unrated".into()),
    }
}

impl SecurityStore {
    pub fn new(graph: Arc<Graph>, ig_id: String, scope: String) -> Self {
        Self { graph, ig_id, scope }
    }

    async fn run(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    async fn indexes(&self) -> Result<(), RpcError> {
        self.graph.ensure_index(&self.ig_id, "security_finding_id", "Finding", "id").await?;
        self.graph.ensure_index(&self.ig_id, "security_finding_status", "Finding", "status").await?;
        self.graph.ensure_index(&self.ig_id, "security_audit_id", "Audit", "id").await
    }

    // ---- audits --------------------------------------------------------------------------------

    pub async fn audit_put(&self, audit: &SecurityAudit) -> Result<(), RpcError> {
        if audit.id.is_empty() {
            return Err(RpcError::invalid_params("an audit needs an id"));
        }
        let mut props = match serde_json::to_value(audit) {
            Ok(Value::Object(map)) => map,
            _ => return Err(RpcError::internal("an audit record is an object")),
        };
        props.insert("project".into(), json!(self.scope));
        props.insert("name".into(), json!(audit.id));
        self.run(&format!("{} MERGE (a:Security:Audit {{id: $id}}) SET a += $props", layer::WORKSPACE_WRITE), params! { "id" => audit.id, "props" => props }).await?;
        self.indexes().await
    }

    async fn audits(&self) -> Result<Vec<SecurityAudit>, RpcError> {
        let rows = self.run(&format!("{} MATCH (a:Security:Audit) RETURN {}", layer::WORKSPACE_READ, columns("a", &AUDIT_FIELDS)), Map::new()).await?;
        rows.rows.iter().map(|row| audit_from(row)).collect()
    }

    // ---- findings ------------------------------------------------------------------------------

    async fn finding_row(&self, id: &str) -> Result<Option<Vec<Value>>, RpcError> {
        let rows = self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}}) RETURN {}", layer::WORKSPACE_READ, columns("f", &FINDING_FIELDS)), params! { "id" => id }).await?;
        Ok(rows.rows.into_iter().next())
    }

    pub async fn finding(&self, id: &str) -> Result<Option<SecurityFindingDetail>, RpcError> {
        let Some(row) = self.finding_row(id).await? else { return Ok(None) };
        let parts = self.parts(&[id.to_owned()]).await?;
        let parts = parts.get(id);
        Ok(Some(SecurityFindingDetail { finding: finding_from(&row, parts)?, history: history_from(&row, parts) }))
    }

    /// Write a finding's node and its edges; `history` is its whole lifecycle so far.
    async fn write(&self, finding: &SecurityFinding, history: &[SecurityStatusChange], now: u64) -> Result<(), RpcError> {
        let mut props = match serde_json::to_value(finding) {
            Ok(Value::Object(map)) => map,
            _ => return Err(RpcError::internal("a finding record is an object")),
        };
        // The trace, quick fix and history are nodes of their own (an earlier version's text goes).
        props.insert("trace".into(), Value::Null);
        props.insert("quick_fix".into(), Value::Null);
        props.insert("history".into(), Value::Null);
        props.insert("project".into(), json!(self.scope));
        props.insert("name".into(), json!(finding.title));
        props.insert("updated_ms".into(), json!(now));
        self.run(
            &format!("{} MERGE (f:Security:Finding {{id: $id}}) SET f += $props", layer::WORKSPACE_WRITE),
            params! { "id" => finding.id, "props" => props },
        )
        .await?;
        self.indexes().await?;
        let key = params! { "id" => finding.id };
        if let Some(audit) = &finding.audit_id {
            self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}}), (a:Security:Audit {{id: $audit}}) MERGE (f)-[:FOUND_BY]->(a)", layer::WORKSPACE_WRITE), params! { "id" => finding.id, "audit" => audit }).await?;
        }
        self.link_trace(finding).await?;
        self.write_parts(finding, history, now).await?;
        self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}})-[r:IN_PACKAGE]->() DELETE r", layer::WORKSPACE_WRITE), key.clone()).await?;
        if let Some(fix) = &finding.quick_fix
            && !fix.package.is_empty()
        {
            self.run(
                &format!("{} MATCH (f:Security:Finding {{id: $id}}), (d:Code:Dependency {{sid: $sid}}) MERGE (f)-[:IN_PACKAGE]->(d)", layer::WORKSPACE_WRITE),
                params! { "id" => finding.id, "sid" => crate::model::dep_sid(&fix.ecosystem, &fix.package) },
            )
            .await?;
        }
        self.link_session(&finding.id, finding.session_id.as_deref()).await
    }

    /// The parts of findings: their trace steps, quick fix and status moves, by finding id.
    async fn parts(&self, ids: &[String]) -> Result<HashMap<String, Parts>, RpcError> {
        let mut out: HashMap<String, Parts> = HashMap::new();
        let rows = self.run(&format!("{} UNWIND $ids AS i MATCH (f:Security:Finding {{id: i}})-[:HAS_STEP]->(t:SecurityTraceStep) RETURN i, t", layer::WORKSPACE_READ), params! { "ids" => ids }).await?;
        let mut steps: HashMap<String, Vec<(u64, SecurityTraceStep)>> = HashMap::new();
        for row in &rows.rows {
            let (Some(id), Some(node)) = (row.first().and_then(Value::as_str), row.get(1)) else { continue };
            let props = props_of(node);
            let position = props.get("position").and_then(Value::as_u64).unwrap_or(0);
            if let Ok(step) = serde_json::from_value::<SecurityTraceStep>(Value::Object(props)) {
                steps.entry(id.to_owned()).or_default().push((position, step));
            }
        }
        for (id, mut list) in steps {
            list.sort_by_key(|(position, _)| *position);
            out.entry(id).or_default().trace = list.into_iter().map(|(_, step)| step).collect();
        }
        let rows = self.run(&format!("{} UNWIND $ids AS i MATCH (f:Security:Finding {{id: i}})-[:QUICK_FIX]->(q:SecurityQuickFix) RETURN i, q", layer::WORKSPACE_READ), params! { "ids" => ids }).await?;
        for row in &rows.rows {
            let (Some(id), Some(node)) = (row.first().and_then(Value::as_str), row.get(1)) else { continue };
            if let Ok(fix) = serde_json::from_value::<SecurityQuickFix>(Value::Object(props_of(node))) {
                out.entry(id.to_owned()).or_default().quick_fix = Some(fix);
            }
        }
        let rows = self.run(&format!("{} UNWIND $ids AS i MATCH (f:Security:Finding {{id: i}})-[:CHANGED]->(c:SecurityStatusChange) RETURN i, c", layer::WORKSPACE_READ), params! { "ids" => ids }).await?;
        let mut moves: HashMap<String, Vec<(u64, SecurityStatusChange)>> = HashMap::new();
        for row in &rows.rows {
            let (Some(id), Some(node)) = (row.first().and_then(Value::as_str), row.get(1)) else { continue };
            let props = props_of(node);
            let position = props.get("position").and_then(Value::as_u64).unwrap_or(0);
            if let Ok(change) = serde_json::from_value::<SecurityStatusChange>(Value::Object(props)) {
                moves.entry(id.to_owned()).or_default().push((position, change));
            }
        }
        for (id, mut list) in moves {
            list.sort_by_key(|(position, _)| *position);
            out.entry(id).or_default().history = list.into_iter().map(|(_, change)| change).collect();
        }
        Ok(out)
    }

    /// Write a finding's trace steps and quick fix (replacing the stored ones) and the status moves
    /// not stored yet, which also go to the `security.status` stream.
    async fn write_parts(&self, finding: &SecurityFinding, history: &[SecurityStatusChange], now: u64) -> Result<(), RpcError> {
        let id = &finding.id;
        self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}})-[:HAS_STEP]->(t:SecurityTraceStep) DETACH DELETE t", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        for (position, step) in finding.trace.iter().enumerate() {
            self.run(
                &format!(
                    "{} MATCH (f:Security:Finding {{id: $finding}}) CREATE (f)-[:HAS_STEP {{first_ms: $now, last_ms: $now}}]->(:SecurityTraceStep {{id: $id, finding_id: $finding, position: $position, kind: $kind, file: $file, line: $line, what: $what, first_ms: $now, last_ms: $now}})",
                    layer::WORKSPACE_WRITE
                ),
                params! { "finding" => id, "id" => format!("{id}#{position}"), "position" => position, "kind" => step.kind, "file" => step.file, "line" => step.line, "what" => step.what, "now" => now },
            )
            .await?;
        }
        self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}})-[:QUICK_FIX]->(q:SecurityQuickFix) DETACH DELETE q", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        if let Some(fix) = &finding.quick_fix {
            self.run(
                &format!(
                    "{} MATCH (f:Security:Finding {{id: $finding}}) CREATE (f)-[:QUICK_FIX {{first_ms: $now, last_ms: $now}}]->(:SecurityQuickFix {{id: $finding, finding_id: $finding, kind: $kind, ecosystem: $ecosystem, package: $package, from: $from, to: $to, lockfile: $lockfile, commands: $commands, first_ms: $now, last_ms: $now}})",
                    layer::WORKSPACE_WRITE
                ),
                params! { "finding" => id, "kind" => fix.kind, "ecosystem" => fix.ecosystem, "package" => fix.package, "from" => fix.from, "to" => fix.to, "lockfile" => fix.lockfile, "commands" => fix.commands, "now" => now },
            )
            .await?;
        }
        let stored = self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}})-[:CHANGED]->(c:SecurityStatusChange) RETURN count(c)", layer::WORKSPACE_READ), params! { "id" => id }).await?;
        let known = stored.rows.first().and_then(|row| row.first()).and_then(Value::as_u64).unwrap_or(0) as usize;
        let mut records = Vec::new();
        for (position, change) in history.iter().enumerate().skip(known) {
            self.run(
                &format!(
                    "{} MATCH (f:Security:Finding {{id: $finding}}) MERGE (c:SecurityStatusChange {{id: $id}}) SET c.finding_id = $finding, c.position = $position, c.status = $status, c.note = $note, c.by = $by, c.ms = $ms, c.session_id = $session, c.commit = $commit, c.first_ms = $ms, c.last_ms = $ms MERGE (f)-[:CHANGED]->(c)",
                    layer::WORKSPACE_WRITE
                ),
                params! {
                    "finding" => id, "id" => format!("{id}#{position}"), "position" => position, "status" => change.status, "note" => change.note, "by" => change.by,
                    "ms" => change.ms, "session" => change.session_id, "commit" => change.commit
                },
            )
            .await?;
            if let Ok(bytes) = serde_json::to_vec(&json!({ "at_ms": change.ms, "value": { "finding": id, "change": change } })) {
                records.push(bytes);
            }
        }
        if !records.is_empty()
            && let Err(error) = self.graph.stream_append(&self.ig_id, STATUS_TOPIC, records).await
        {
            tracing::warn!(error = %error.message, "status moves not added to their stream");
        }
        Ok(())
    }

    /// Replace a finding's `AT` edges: one per trace step whose file and line are indexed code.
    async fn link_trace(&self, finding: &SecurityFinding) -> Result<(), RpcError> {
        self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}})-[r:AT]->() DELETE r", layer::WORKSPACE_WRITE), params! { "id" => finding.id }).await?;
        for (step, place) in finding.trace.iter().enumerate() {
            let Some(target) = self.code_at(&place.file, place.line).await? else { continue };
            self.run(
                &format!("{} MATCH (f:Security:Finding {{id: $id}}), (c:Code {{sid: $target}}) CREATE (f)-[:AT {{step: $step, kind: $kind, what: $what, line: $line}}]->(c)", layer::WORKSPACE_WRITE),
                params! { "id" => finding.id, "target" => target, "step" => step, "kind" => place.kind, "what" => place.what, "line" => place.line },
            )
            .await?;
        }
        Ok(())
    }

    /// The sid of the innermost symbol whose range covers `line` in `path`, else the file's.
    async fn code_at(&self, path: &str, line: u32) -> Result<Option<String>, RpcError> {
        let path = path.trim_start_matches("./");
        let rows = self.run(&format!("{} MATCH (n:Code {{path: $p}}) WHERE n:Sym OR n:File RETURN n.sid, n.kind, n.start_line, n.end_line", layer::OBSERVED_READ), params! { "p" => path }).await?;
        let number = |value: Option<&Value>| value.and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0);
        let mut file = None;
        let mut innermost: Option<(u64, String)> = None;
        for row in &rows.rows {
            let sid = row.first().and_then(|v| v.as_str()).unwrap_or("").to_owned();
            if row.get(1).and_then(|v| v.as_str()) == Some("file") {
                file = Some(sid);
                continue;
            }
            let (start, end) = (number(row.get(2)), number(row.get(3)));
            let covers = start <= u64::from(line) && u64::from(line) <= end.max(start);
            if covers && innermost.as_ref().is_none_or(|(span, _)| end - start < *span) {
                innermost = Some((end - start, sid));
            }
        }
        Ok(innermost.map(|(_, sid)| sid).or(file))
    }

    /// Replace a finding's `FIXED_IN` edge with one to the session, when the session is a node here.
    async fn link_session(&self, id: &str, session: Option<&str>) -> Result<(), RpcError> {
        self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}})-[r:FIXED_IN]->() DELETE r", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        if let Some(session) = session.filter(|session| !session.is_empty()) {
            self.run(
                &format!("{} MATCH (f:Security:Finding {{id: $id}}), (s:Session {{id: $session}}) MERGE (f)-[:FIXED_IN]->(s)", layer::WORKSPACE_WRITE),
                params! { "id" => id, "session" => session },
            )
            .await?;
        }
        Ok(())
    }

    /// Create or update findings by id. A new finding is open; a known one keeps its lifecycle and
    /// takes the new content; one that was fixed, verified or gone is open again. Returns them as stored.
    pub async fn findings_put(&self, audit_id: Option<&str>, findings: Vec<SecurityFinding>) -> Result<Vec<SecurityFinding>, RpcError> {
        let now = now_ms();
        let audit = audit_id.map(str::to_owned);
        let audit_name = audit.clone().unwrap_or_default();
        let mut stored = Vec::with_capacity(findings.len());
        for mut finding in findings {
            if finding.id.is_empty() {
                return Err(RpcError::invalid_params("a finding needs an id (its fingerprint)"));
            }
            if let Some(sink) = finding.trace.last() {
                finding.sink_path.get_or_insert_with(|| sink.file.clone());
                finding.sink_line.get_or_insert(sink.line);
            }
            let mut history: Vec<SecurityStatusChange>;
            match self.finding_row(&finding.id).await? {
                None => {
                    finding.status = "open".into();
                    finding.status_by = "model".into();
                    finding.status_note = String::new();
                    finding.status_ms = now;
                    finding.first_seen_ms = now;
                    finding.seen_in = 1;
                    finding.session_id = None;
                    finding.fixed_commit = None;
                    history = vec![SecurityStatusChange { status: "open".into(), by: "model".into(), ms: now, note: format!("found by audit {audit_name}"), session_id: None, commit: None }];
                }
                Some(row) => {
                    let parts = self.parts(std::slice::from_ref(&finding.id)).await?;
                    let parts = parts.get(&finding.id);
                    let known = finding_from(&row, parts)?;
                    history = history_from(&row, parts);
                    finding.status = known.status;
                    finding.status_note = known.status_note;
                    finding.status_by = known.status_by;
                    finding.status_ms = known.status_ms;
                    finding.first_seen_ms = known.first_seen_ms;
                    finding.seen_in = known.seen_in + 1;
                    finding.session_id = known.session_id;
                    finding.fixed_commit = known.fixed_commit;
                    // Reported again after it was closed: the fix did not hold, or the code came back.
                    if matches!(finding.status.as_str(), "fixed" | "verified" | "gone") {
                        let change = SecurityStatusChange { status: "open".into(), by: "recheck".into(), ms: now, note: format!("reported again by audit {audit_name}"), session_id: None, commit: None };
                        finding.status = change.status.clone();
                        finding.status_by = change.by.clone();
                        finding.status_note = change.note.clone();
                        finding.status_ms = now;
                        history.push(change);
                    }
                }
            }
            finding.last_seen_ms = now;
            finding.audit_id = audit.clone();
            self.write(&finding, &history, now).await?;
            stored.push(finding);
        }
        Ok(stored)
    }

    /// The project's findings in the asked statuses (active ones by default), highest priority
    /// first, the oldest first among equals.
    pub async fn findings(&self, p: &SecurityFindingsParams) -> Result<Vec<SecurityFinding>, RpcError> {
        let statuses: Vec<String> = match &p.status {
            Some(list) if !list.is_empty() => list.clone(),
            _ => ACTIVE.iter().map(|s| (*s).to_owned()).collect(),
        };
        let rows = self
            .run(
                &format!("{} UNWIND $statuses AS st MATCH (f:Security:Finding {{status: st}}) RETURN {}", layer::WORKSPACE_READ, columns("f", &FINDING_FIELDS)),
                params! { "statuses" => statuses },
            )
            .await?;
        let ids: Vec<String> = rows.column(0).filter_map(|id| id.as_str().map(str::to_owned)).collect();
        let parts = self.parts(&ids).await?;
        let mut findings = rows
            .rows
            .iter()
            .map(|row| finding_from(row, row.first().and_then(Value::as_str).and_then(|id| parts.get(id))))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|f| p.class.as_ref().is_none_or(|class| &f.class == class))
            .filter(|f| p.min_priority.is_none_or(|min| f.priority >= min))
            .collect::<Vec<_>>();
        findings.sort_by(|a, b| (std::cmp::Reverse(a.priority), a.first_seen_ms, &a.id).cmp(&(std::cmp::Reverse(b.priority), b.first_seen_ms, &b.id)));
        Ok(findings)
    }

    /// A lifecycle move, refused when the lifecycle does not allow it.
    pub async fn finding_update(&self, p: SecurityFindingUpdateParams) -> Result<Option<SecurityFinding>, RpcError> {
        let Some(row) = self.finding_row(&p.id).await? else { return Ok(None) };
        let parts = self.parts(std::slice::from_ref(&p.id)).await?;
        let parts = parts.get(&p.id);
        let mut finding = finding_from(&row, parts)?;
        let mut history = history_from(&row, parts);
        let current = finding.status.clone();
        let moves = allowed_moves(&current);
        let recheck_only = current == "open" && p.status == "fixed" && p.by != "recheck";
        if !moves.contains(&p.status.as_str()) || recheck_only {
            let allowed: Vec<String> = moves.iter().map(|m| if current == "open" && *m == "fixed" { "fixed (by a re-check)".to_owned() } else { (*m).to_owned() }).collect();
            return Err(RpcError::invalid_params(format!("finding {} is {current}; it can move to {}, not {}", p.id, allowed.join(", "), p.status)));
        }
        if p.status == "fixing" && p.session_id.as_deref().is_none_or(str::is_empty) {
            return Err(RpcError::invalid_params(format!("finding {} moves to fixing with the session that fixes it: pass session_id", p.id)));
        }
        let now = now_ms();
        finding.status = p.status.clone();
        finding.status_note = p.note.clone();
        finding.status_by = p.by.clone();
        finding.status_ms = now;
        if let Some(session) = p.session_id.clone().filter(|s| !s.is_empty()) {
            finding.session_id = Some(session);
        }
        if let Some(commit) = p.commit.clone().filter(|c| !c.is_empty()) {
            finding.fixed_commit = Some(commit);
        }
        history.push(SecurityStatusChange { status: p.status, note: p.note, by: p.by, ms: now, session_id: p.session_id, commit: p.commit });
        self.write(&finding, &history, now).await?;
        Ok(Some(finding))
    }

    pub async fn summary(&self) -> Result<SecuritySummary, RpcError> {
        let rows = self.run(&format!("{} MATCH (f:Security:Finding) RETURN {}", layer::WORKSPACE_READ, columns("f", &FINDING_FIELDS)), Map::new()).await?;
        let mut summary = SecuritySummary::default();
        for row in &rows.rows {
            let finding = finding_from(row, None)?;
            match finding.status.as_str() {
                "open" => summary.open += 1,
                "fixing" => summary.fixing += 1,
                "accepted" => summary.accepted += 1,
                "fixed" => summary.fixed += 1,
                "verified" => summary.verified += 1,
                _ => {}
            }
            if matches!(finding.status.as_str(), "open" | "fixing") {
                *summary.by_severity.entry(severity_key(&finding)).or_insert(0) += 1;
            }
        }
        summary.last_audit = self.audits().await?.into_iter().max_by_key(|audit| (audit.started_ms, audit.id.clone()));
        Ok(summary)
    }

    /// Delete a finding with its edges and history. Returns whether there was one.
    pub async fn forget(&self, id: &str) -> Result<bool, RpcError> {
        if self.finding_row(id).await?.is_none() {
            return Ok(false);
        }
        for label in ["SecurityTraceStep", "SecurityQuickFix", "SecurityStatusChange"] {
            self.run(&format!("{} MATCH (n:{label} {{finding_id: $id}}) DETACH DELETE n", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        }
        self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}}) DETACH DELETE f", layer::WORKSPACE_WRITE), params! { "id" => id }).await?;
        Ok(true)
    }
}
