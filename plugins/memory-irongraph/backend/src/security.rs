//! Security in the WORKSPACE layer: the security plugin's audit runs (their map, units, the lines
//! they judge and the vulnerable dependencies they found) and the findings they report, with each
//! finding's lifecycle (status, who moved it, its history) and the evidence the host checks a fix
//! against.
//!
//! ```text
//! (:Security:Audit {id, project, ...SecurityAudit's fields})-[:AFTER]->(:Security:Audit)
//! (:Security:Audit)-[:IN]->(r:Code:Repo {sid: 'repo:'})   (:Security:Finding)-[:IN]->(r)   the project's root
//! (:Security:Unit {id: audit#U1, audit_id, unit, ...SecurityUnit})-[:OF]->(a:Audit)
//! (:Security:Hit {id: audit#U1.3, audit_id, hit, ...SecurityHit})-[:IN]->(u:Unit)
//! (h:Hit)-[:AT]->(c:Code)                         the innermost symbol at the line, else the file
//! (:Security:Advisory {id: audit#eco:pkg:version:advisory, audit_id, ...SecurityAdvisory})-[:OF]->(a)
//! (:Security:Advisory)-[:IN_PACKAGE]->(d:Code:Dependency)
//! (:Security:Finding {id, project, ...SecurityFinding's scalar fields})
//! (f)-[:HAS_STEP]->(:SecurityTraceStep {id, finding_id, position, kind, file, line, what})
//! (f)-[:QUICK_FIX]->(:SecurityQuickFix {id, finding_id, kind, ecosystem, package, from, to, lockfile, commands})
//! (f)-[:CHANGED]->(:SecurityStatusChange {id, finding_id, position, status, note, by, ms, session_id, commit})
//! (f)-[:FOUND_BY]->(a:Audit)                     every audit that reported it
//! (f)-[:AT {step, kind, what, line}]->(c:Code)   each trace step's innermost symbol, else its file
//! (f)-[:IN_PACKAGE]->(d:Code:Dependency)         the package a quick fix upgrades
//! (f)-[:FIXED_IN]->(s:Session)                   the session that fixes or fixed it
//! ```
//!
//! A finding is identified by its fingerprint, so an audit that reports it again updates the node
//! instead of adding one. Nothing is deleted: an audit a newer one replaced, a forgotten finding and
//! an advisory a later check no longer reports are retired (`ended_ms`); a trace step or quick
//! fix a newer report replaced is retired too (`ended_ms`, `ended_reason`, `state: 'gone'`), and an
//! edge that no longer holds (`AT`, `IN_PACKAGE`, `FIXED_IN`) gets `ended_ms`. Reads skip them; a
//! write that names them again opens them. Every node and edge keeps `first_ms` and `last_ms`.
//!
//! **Commands.** A call from a command in the model's shells carries `_command_session` (the frame
//! stamps it). Its moves are the agent's (`by: agent`, or `recheck`); the person's moves (accept the
//! risk, not an issue, a reopen, a forget) are refused; a `fixed` is accepted only when the sink's
//! evidence (the line's hash, or the pinned package version) is gone from the working tree's file.
//! A hit with a strong host signal is never settled `safe` or `hardening`.

use crate::db::{Graph, layer};
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{
    SecurityAdvisory, SecurityAudit, SecurityAuditStartParams, SecurityAuditState, SecurityFinding, SecurityFindingDetail, SecurityFindingUpdateParams, SecurityFindingsParams, SecurityHit,
    SecurityQuickFix, SecurityRecheck, SecurityRecheckParams, SecurityStatusChange, SecuritySummary, SecurityTraceStep, SecurityUnit,
};
use cc_protocol::contracts::now_ms;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The statuses `memory.security.findings` lists when the caller names none: the work still open.
const ACTIVE: [&str; 3] = ["open", "fixing", "accepted"];

/// Every status, for the callers that ask by id or by audit.
const STATUSES: [&str; 6] = ["open", "fixing", "accepted", "fixed", "verified", "gone"];

/// Finding properties, in the order `finding_from` reads them (the JSON field names of
/// [`SecurityFinding`], plus `history`, `project`, `name`, `updated_ms`).
const FINDING_FIELDS: &[&str] = &[
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
    "blocker",
    "check_how",
    "reason",
    "conditions",
    "trace",
    "quick_fix",
    "priority",
    "priority_reason",
    "source",
    "unit",
    "hit",
    "signal",
    "host_severity",
    "advisories",
    "kev",
    "epss",
    "epss_percentile",
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
    "sink_text",
    "sink_hash",
    "sink_package",
    "sink_version",
    "notified_ms",
    "ended_ms",
    "history",
    "project",
    "name",
    "updated_ms",
];

/// Audit properties (the JSON field names of [`SecurityAudit`]).
const AUDIT_FIELDS: &[&str] = &[
    "id",
    "started_ms",
    "finished_ms",
    "units",
    "lines_judged",
    "findings",
    "leads",
    "hardening",
    "scope",
    "skill_version",
    "report_path",
    "root",
    "git_head",
    "dirty",
    "files",
    "langs",
    "stack",
    "entries",
    "manifests",
    "lockfiles",
    "not_parsed",
    "ci",
    "containers",
    "iac",
    "secret_files",
    "agent_files",
    "tests",
    "deps_checked_ms",
    "deps_packages",
    "deps_note",
    "previous_id",
    "ended_ms",
];

/// Unit properties; `unit` is the unit's own id (`U1`), `id` the node's key.
const UNIT_FIELDS: &[&str] = &["unit", "class", "name", "reason", "status", "note", "error", "hits", "files", "previous_status", "previous_note"];

/// Hit properties; `hit` is the hit's own id (`U1.3`), `id` the node's key.
const HIT_FIELDS: &[&str] = &[
    "hit",
    "unit",
    "class",
    "path",
    "line",
    "shape",
    "text",
    "signal",
    "severity",
    "ecosystem",
    "package",
    "version",
    "verdict",
    "note",
    "record",
    "cited",
    "finding_id",
    "settled_ms",
];

/// What a later settle may change on a hit; the rest stays as the audit first wrote it.
const HIT_SETTLED_FIELDS: &[&str] = &["verdict", "note", "record", "cited", "finding_id", "settled_ms"];

/// Advisory properties (the JSON field names of [`SecurityAdvisory`]).
const ADVISORY_FIELDS: &[&str] = &["ecosystem", "package", "version", "lockfile", "advisory", "aliases", "severity", "summary", "fixed", "cwes", "kev", "epss", "epss_percentile"];

/// Rows per write statement.
const CHUNK: usize = 500;

/// The largest file the host reads to check a sink.
const MAX_SINK_FILE: u64 = 8 * 1024 * 1024;

/// A day, for the age term of the priority.
const DAY_MS: u64 = 86_400_000;

/// Who calls: a command in the model's shells (its session), or the app (the person, a plugin).
#[derive(Debug, Clone, Default)]
pub struct Caller {
    pub command_session: Option<String>,
}

impl Caller {
    fn command(&self) -> bool {
        self.command_session.is_some()
    }
}

/// What a write produced beyond the records: notes for the caller and findings to tell the person about.
#[derive(Debug, Default)]
pub struct PutOutcome {
    pub findings: Vec<SecurityFinding>,
    pub notes: Vec<String>,
    pub notify: Vec<SecurityFinding>,
}

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

/// A record as node properties: its scalar and list fields (`null` removes a property).
fn to_props<T: Serialize>(value: &T, what: &str) -> Result<Map<String, Value>, RpcError> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(RpcError::internal(format!("a {what} record is an object"))),
    }
}

/// A finding's parts kept as nodes of their own.
#[derive(Default)]
struct Parts {
    trace: Vec<SecurityTraceStep>,
    quick_fix: Option<SecurityQuickFix>,
    history: Vec<SecurityStatusChange>,
}

fn finding_from(row: &[Value], parts: Option<&Parts>) -> Result<SecurityFinding, RpcError> {
    let mut object = object(row, FINDING_FIELDS);
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
        _ => from_text(&object(row, FINDING_FIELDS), "history").unwrap_or_default(),
    }
}

fn props_of(node: &Value) -> Map<String, Value> {
    node.get("properties").and_then(Value::as_object).cloned().unwrap_or_default().into_iter().map(|(k, v)| (k, whole(v))).collect()
}

fn audit_from(row: &[Value]) -> Result<SecurityAudit, RpcError> {
    serde_json::from_value(Value::Object(object(row, AUDIT_FIELDS))).map_err(|error| RpcError::internal(format!("unreadable audit record: {error}")))
}

/// A record whose own id is kept under `own` (the node's `id` is its key in the graph).
fn keyed_from<T: serde::de::DeserializeOwned>(row: &[Value], fields: &[&str], own: &str, what: &str) -> Result<T, RpcError> {
    let mut object = object(row, fields);
    if let Some(id) = object.remove(own) {
        object.insert("id".into(), id);
    }
    serde_json::from_value(Value::Object(object)).map_err(|error| RpcError::internal(format!("unreadable {what} record: {error}")))
}

fn advisory_from(row: &[Value]) -> Result<SecurityAdvisory, RpcError> {
    serde_json::from_value(Value::Object(object(row, ADVISORY_FIELDS))).map_err(|error| RpcError::internal(format!("unreadable advisory record: {error}")))
}

/// `U3.12` -> (3, 12); `U0` -> (0, 0): the order units and hits are read in.
fn order_of(id: &str) -> (u32, u32) {
    let id = id.trim_start_matches(['U', 'u']);
    let mut parts = id.splitn(2, '.');
    let unit = parts.next().and_then(|p| p.parse().ok()).unwrap_or(u32::MAX);
    let line = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (unit, line)
}

/// The rank of a severity word (higher is worse); unknown words rank below `informational`.
fn severity_rank(severity: Option<&str>) -> i32 {
    match severity.unwrap_or("").to_ascii_lowercase().as_str() {
        "critical" => 4,
        "high" => 3,
        "medium" | "moderate" => 2,
        "low" => 1,
        "informational" => 0,
        _ => -1,
    }
}

/// The moves the lifecycle allows from a status (see [`SecurityFinding`]).
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

/// The priority as read: the writer's number plus one per day the finding has been open (at
/// most 30), while it is still to do.
fn with_age(mut finding: SecurityFinding, now: u64) -> SecurityFinding {
    if ACTIVE.contains(&finding.status.as_str()) && finding.first_seen_ms > 0 && now > finding.first_seen_ms {
        let days = ((now - finding.first_seen_ms) / DAY_MS).min(30) as i64;
        if days > 0 {
            finding.priority += days;
            let term = format!("open {days} day{} +{days}", if days == 1 { "" } else { "s" });
            finding.priority_reason = if finding.priority_reason.is_empty() { term } else { format!("{}, {term}", finding.priority_reason) };
        }
    }
    finding
}

// ---- the sink's evidence ------------------------------------------------------------------------

/// A line as the evidence compares it: ASCII whitespace runs collapsed to one space, trimmed.
pub fn normalize(line: &str) -> String {
    line.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

/// The SHA-256 (hex) of a normalized line; `None` for a line too short to tell apart.
pub fn line_hash(line: &str) -> Option<String> {
    let normal = normalize(line);
    if normal.chars().count() < 3 {
        return None;
    }
    Some(Sha256::digest(normal.as_bytes()).iter().map(|b| format!("{b:02x}")).collect())
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Whether `token` stands in `line` with no `before` character just before it and no `after`
/// character just after it.
fn has_token(line: &str, token: &str, before: impl Fn(char) -> bool, after: impl Fn(char) -> bool) -> bool {
    if token.is_empty() {
        return false;
    }
    line.match_indices(token).any(|(at, _)| {
        let prev = line[..at].chars().next_back();
        let next = line[at + token.len()..].chars().next();
        prev.is_none_or(|c| !before(c)) && next.is_none_or(|c| !after(c))
    })
}

/// The 1-based line of a lockfile that pins `package` at `version`: the package's name, with the
/// version on its line or within the seven lines after it (the layouts of npm, yarn, pnpm, Cargo,
/// Poetry, Go, Bundler, Composer, Maven, pip). `None` when nothing pins it there.
pub fn pin_line(text: &str, package: &str, version: &str) -> Option<u32> {
    let name = package.rsplit(':').next().unwrap_or(package);
    let version = version.trim_start_matches('v');
    if name.is_empty() || version.is_empty() {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if !has_token(line, name, name_char, |c| name_char(c) || c == '/') {
            continue;
        }
        let end = (index + 8).min(lines.len());
        if lines[index..end].iter().any(|l| has_token(l, version, |c| c.is_ascii_digit() || c == '.', |c| c.is_ascii_alphanumeric() || c == '.')) {
            return Some(index as u32 + 1);
        }
    }
    None
}

/// A repository-relative path inside `root`, or `None` when it would leave it.
fn workspace_file(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.trim_start_matches("./");
    let path = Path::new(rel);
    if rel.is_empty() || path.is_absolute() || path.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return None;
    }
    let root = std::fs::canonicalize(root).ok()?;
    let full = root.join(path);
    match std::fs::canonicalize(&full) {
        Ok(real) if real.starts_with(&root) => Some(real),
        Ok(_) => None,
        // Not there (any more): the caller sees that it does not exist.
        Err(_) => Some(full),
    }
}

fn read_sink_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SINK_FILE {
        return None;
    }
    std::fs::read(path).ok().map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// What the working tree shows of a finding's sink now.
#[derive(Debug, Clone, PartialEq)]
enum Evidence {
    Gone(String),
    At { line: u32, moved: bool, sentence: String },
    Unknown(String),
}

impl Evidence {
    fn kind(&self) -> &'static str {
        match self {
            Evidence::Gone(_) => "gone",
            Evidence::At { moved: false, .. } => "unchanged",
            Evidence::At { moved: true, .. } => "moved",
            Evidence::Unknown(_) => "unknown",
        }
    }

    fn sentence(&self) -> &str {
        match self {
            Evidence::Gone(sentence) | Evidence::Unknown(sentence) | Evidence::At { sentence, .. } => sentence,
        }
    }

    fn line(&self) -> Option<u32> {
        match self {
            Evidence::At { line, .. } => Some(*line),
            _ => None,
        }
    }
}

/// Compare a finding's stored evidence with the file in `root`.
fn evidence_now(root: &Path, finding: &SecurityFinding) -> Evidence {
    let Some(rel) = finding.sink_path.as_deref().filter(|p| !p.is_empty()) else {
        return Evidence::Unknown("the finding names no sink to compare".into());
    };
    let Some(path) = workspace_file(root, rel) else {
        return Evidence::Unknown(format!("the sink {rel} is outside the working tree"));
    };
    if !path.exists() {
        return Evidence::Gone(format!("{rel} no longer exists"));
    }
    let Some(text) = read_sink_file(&path) else {
        return Evidence::Unknown(format!("{rel} cannot be read as text"));
    };
    if let (Some(package), Some(version)) = (finding.sink_package.as_deref(), finding.sink_version.as_deref()) {
        return match pin_line(&text, package, version) {
            Some(line) => Evidence::At { line, moved: false, sentence: format!("{package} is still pinned at {version} in {rel}:{line}") },
            None => Evidence::Gone(format!("{package} {version} is no longer pinned in {rel}")),
        };
    }
    let Some(hash) = finding.sink_hash.as_deref() else {
        return Evidence::Unknown("no evidence of the sink is stored; the next audit records it".into());
    };
    let found: Vec<u32> = text.lines().enumerate().filter(|(_, line)| line_hash(line).as_deref() == Some(hash)).map(|(index, _)| index as u32 + 1).collect();
    let wanted = finding.sink_line.unwrap_or(0);
    if found.contains(&wanted) {
        return Evidence::At { line: wanted, moved: false, sentence: format!("the flagged line is unchanged at {rel}:{wanted}") };
    }
    match found.first() {
        Some(line) => Evidence::At { line: *line, moved: true, sentence: format!("the flagged line moved to {rel}:{line}; it is still there") },
        None => Evidence::Gone(format!("the flagged line is gone from {rel}")),
    }
}

/// Fill a finding's evidence from the file as it is now: the sink line's hash, or for a dependency
/// the line that pins the package.
fn record_evidence(root: &Path, finding: &mut SecurityFinding) {
    let Some(rel) = finding.sink_path.clone().filter(|p| !p.is_empty()) else { return };
    let Some(text) = workspace_file(root, &rel).and_then(|path| read_sink_file(&path)) else { return };
    if let (Some(package), Some(version)) = (finding.sink_package.as_deref(), finding.sink_version.as_deref()) {
        if let Some(line) = pin_line(&text, package, version) {
            finding.sink_line = Some(line);
        }
        return;
    }
    let Some(line) = finding.sink_line.filter(|line| *line > 0) else { return };
    if let Some(hash) = text.lines().nth(line as usize - 1).and_then(line_hash) {
        finding.sink_hash = Some(hash);
    }
}

impl SecurityStore {
    pub fn new(graph: Arc<Graph>, ig_id: String, scope: String) -> Self {
        Self { graph, ig_id, scope }
    }

    async fn run(&self, cypher: &str, params: Map<String, Value>) -> Result<crate::db::Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    /// Link the node `matched` binds as `w` to the project's code root (made, keyed as the code
    /// tree keys it, when no sync has made it yet), so no audit or finding stands alone.
    async fn link_root(&self, matched: &str, mut p: Map<String, Value>) -> Result<(), RpcError> {
        let now = now_ms();
        p.insert("now".into(), json!(now));
        let found = self.run(&format!("{} MATCH (r:Repo {{sid: 'repo:'}}) RETURN r.sid LIMIT 1", layer::OBSERVED_READ), Map::new()).await?;
        if found.rows.is_empty() {
            self.run(
                &format!(
                    "{} MERGE (r:Code:Repo {{sid: 'repo:'}}) ON CREATE SET r.kind = 'repo', r.name = $name, r.qname = '', r.path = '' SET r.first_ms = CASE WHEN r.first_ms IS NULL THEN $now ELSE r.first_ms END, r.last_ms = $now",
                    layer::OBSERVED_WRITE
                ),
                params! { "name" => self.scope, "now" => now },
            )
            .await?;
        }
        self.run(
            &format!(
                "{} {matched} MATCH (root:Repo {{sid: 'repo:'}}) MERGE (w)-[e:IN]->(root) SET e.first_ms = CASE WHEN e.first_ms IS NULL THEN $now ELSE e.first_ms END, e.last_ms = $now",
                layer::WORKSPACE_WRITE
            ),
            p,
        )
        .await?;
        Ok(())
    }

    /// Mark the live `rel` edges of finding `id` as ended (a write after this opens the ones that
    /// still hold).
    async fn end_edges(&self, id: &str, rel: &str, now: u64) -> Result<(), RpcError> {
        self.run(
            &format!("{} MATCH (f:Security:Finding {{id: $id}})-[r:{rel}]->() WHERE r.ended_ms IS NULL SET r.ended_ms = $now, r.last_ms = $now", layer::WORKSPACE_WRITE),
            params! { "id" => id, "now" => now },
        )
        .await?;
        Ok(())
    }

    async fn indexes(&self) -> Result<(), RpcError> {
        self.graph.ensure_index(&self.ig_id, "security_finding_id", "Finding", "id").await?;
        self.graph.ensure_index(&self.ig_id, "security_finding_status", "Finding", "status").await?;
        self.graph.ensure_index(&self.ig_id, "security_audit_id", "Audit", "id").await?;
        self.graph.ensure_index(&self.ig_id, "security_unit_id", "Unit", "id").await?;
        self.graph.ensure_index(&self.ig_id, "security_unit_audit", "Unit", "audit_id").await?;
        self.graph.ensure_index(&self.ig_id, "security_hit_id", "Hit", "id").await?;
        self.graph.ensure_index(&self.ig_id, "security_hit_audit", "Hit", "audit_id").await?;
        self.graph.ensure_index(&self.ig_id, "security_advisory_id", "Advisory", "id").await?;
        self.graph.ensure_index(&self.ig_id, "security_advisory_audit", "Advisory", "audit_id").await
    }

    // ---- audits --------------------------------------------------------------------------------

    /// Update an audit's record (its counts, finish and report). The map's facts it does not carry
    /// are kept.
    pub async fn audit_put(&self, audit: &SecurityAudit) -> Result<(), RpcError> {
        if audit.id.is_empty() {
            return Err(RpcError::invalid_params("an audit needs an id"));
        }
        let mut props = to_props(audit, "audit")?;
        // An empty list or a missing value in an update is "not said", never "erase the map".
        props.retain(|_, value| !value.is_null() && value.as_array().is_none_or(|list| !list.is_empty()));
        props.insert("project".into(), json!(self.scope));
        props.insert("name".into(), json!(audit.id));
        props.insert("last_ms".into(), json!(now_ms()));
        self.run(
            &format!("{} MERGE (a:Security:Audit {{id: $id}}) SET a += $props, a.first_ms = CASE WHEN a.first_ms IS NULL THEN $now ELSE a.first_ms END", layer::WORKSPACE_WRITE),
            params! { "id" => audit.id, "props" => props, "now" => now_ms() },
        )
        .await?;
        self.indexes().await?;
        self.link_root("MATCH (w:Security:Audit {id: $id})", params! { "id" => audit.id }).await
    }

    /// Begin an audit: retire the project's earlier ones, write the new one with its units and
    /// advisories, link it to the one before it.
    pub async fn audit_start(&self, p: SecurityAuditStartParams) -> Result<SecurityAuditState, RpcError> {
        let mut audit = p.audit;
        if audit.id.is_empty() {
            return Err(RpcError::invalid_params("an audit needs an id"));
        }
        let now = now_ms();
        let previous = self.audits().await?.into_iter().filter(|a| a.id != audit.id).max_by_key(|a| (a.started_ms, a.id.clone()));
        self.run(
            &format!("{} MATCH (a:Security:Audit) WHERE a.ended_ms IS NULL AND a.id <> $id SET a.ended_ms = $now, a.ended_reason = 'a newer audit began', a.state = 'gone', a.last_ms = $now", layer::WORKSPACE_WRITE),
            params! { "id" => audit.id, "now" => now },
        )
        .await?;
        audit.previous_id = previous.as_ref().map(|a| a.id.clone());
        audit.ended_ms = None;
        let mut props = to_props(&audit, "audit")?;
        props.insert("project".into(), json!(self.scope));
        props.insert("name".into(), json!(audit.id));
        props.insert("first_ms".into(), json!(now));
        props.insert("last_ms".into(), json!(now));
        props.insert("ended_reason".into(), Value::Null);
        props.insert("state".into(), Value::Null);
        self.run(&format!("{} MERGE (a:Security:Audit {{id: $id}}) SET a += $props", layer::WORKSPACE_WRITE), params! { "id" => audit.id, "props" => props }).await?;
        self.indexes().await?;
        self.link_root("MATCH (w:Security:Audit {id: $id})", params! { "id" => audit.id }).await?;
        if let Some(previous) = &previous {
            self.run(
                &format!("{} MATCH (a:Security:Audit {{id: $id}}), (b:Security:Audit {{id: $previous}}) MERGE (a)-[e:AFTER]->(b) SET e.first_ms = CASE WHEN e.first_ms IS NULL THEN $now ELSE e.first_ms END, e.last_ms = $now", layer::WORKSPACE_WRITE),
                params! { "id" => audit.id, "previous" => previous.id, "now" => now },
            )
            .await?;
        }
        self.units_put(&audit.id, &p.units).await?;
        self.advisories_put(&audit.id, &p.advisories).await?;
        self.audit_state(Some(&audit.id), None).await
    }

    async fn audits(&self) -> Result<Vec<SecurityAudit>, RpcError> {
        let rows = self.run(&format!("{} MATCH (a:Security:Audit) RETURN {}", layer::WORKSPACE_READ, columns("a", AUDIT_FIELDS)), Map::new()).await?;
        rows.rows.iter().map(|row| audit_from(row)).collect()
    }

    /// An audit (the current one when none is named) with its units, hits and advisories.
    pub async fn audit_state(&self, audit_id: Option<&str>, units: Option<&[String]>) -> Result<SecurityAuditState, RpcError> {
        let audits = self.audits().await?;
        let audit = match audit_id {
            Some(id) => audits.iter().find(|a| a.id == id).cloned(),
            None => audits.iter().filter(|a| a.ended_ms.is_none()).max_by_key(|a| (a.started_ms, a.id.clone())).cloned(),
        };
        let Some(audit) = audit else { return Ok(SecurityAuditState::default()) };
        let previous = audit.previous_id.as_ref().and_then(|id| audits.iter().find(|a| &a.id == id).cloned());
        let key = params! { "audit" => audit.id };
        let rows = self.run(&format!("{} MATCH (u:Security:Unit {{audit_id: $audit}}) RETURN {}", layer::WORKSPACE_READ, columns("u", UNIT_FIELDS)), key.clone()).await?;
        let mut unit_list = rows.rows.iter().map(|row| keyed_from::<SecurityUnit>(row, UNIT_FIELDS, "unit", "unit")).collect::<Result<Vec<_>, _>>()?;
        unit_list.sort_by_key(|u| order_of(&u.id));
        let rows = self.run(&format!("{} MATCH (h:Security:Hit {{audit_id: $audit}}) RETURN {}", layer::WORKSPACE_READ, columns("h", HIT_FIELDS)), key.clone()).await?;
        let wanted: Option<HashSet<String>> = units.map(|list| list.iter().map(|u| u.to_ascii_uppercase()).collect());
        let mut hits = rows
            .rows
            .iter()
            .map(|row| keyed_from::<SecurityHit>(row, HIT_FIELDS, "hit", "hit"))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|hit| wanted.as_ref().is_none_or(|set| set.contains(&hit.unit.to_ascii_uppercase())))
            .collect::<Vec<_>>();
        hits.sort_by_key(|h| order_of(&h.id));
        let rows = self
            .run(&format!("{} MATCH (v:Security:Advisory {{audit_id: $audit}}) WHERE v.ended_ms IS NULL RETURN {}", layer::WORKSPACE_READ, columns("v", ADVISORY_FIELDS)), key)
            .await?;
        let mut advisories = rows.rows.iter().map(|row| advisory_from(row)).collect::<Result<Vec<_>, _>>()?;
        advisories.sort_by(|a, b| (severity_rank(Some(b.severity.as_str())), &a.package, &a.advisory).cmp(&(severity_rank(Some(a.severity.as_str())), &b.package, &b.advisory)));
        Ok(SecurityAuditState { audit: Some(audit), previous, units: unit_list, hits, advisories })
    }

    async fn audit_exists(&self, audit_id: &str) -> Result<(), RpcError> {
        let rows = self.run(&format!("{} MATCH (a:Security:Audit {{id: $id}}) RETURN a.id", layer::WORKSPACE_READ), params! { "id" => audit_id }).await?;
        if rows.rows.is_empty() {
            return Err(RpcError::not_found(format!("no audit {audit_id} in this project; begin one with memory.security.audit.start")));
        }
        Ok(())
    }

    /// Create or update units by id, each linked to its audit.
    pub async fn units_put(&self, audit_id: &str, units: &[SecurityUnit]) -> Result<(), RpcError> {
        if units.is_empty() {
            return Ok(());
        }
        self.audit_exists(audit_id).await?;
        let now = now_ms();
        let mut rows = Vec::with_capacity(units.len());
        for unit in units {
            if unit.id.is_empty() || unit.class.is_empty() {
                return Err(RpcError::invalid_params("a unit needs an id and a class"));
            }
            let mut props = to_props(unit, "unit")?;
            props.remove("id");
            props.insert("unit".into(), json!(unit.id));
            props.insert("audit_id".into(), json!(audit_id));
            props.insert("last_ms".into(), json!(now));
            rows.push(json!({"key": format!("{audit_id}#{}", unit.id), "props": props}));
        }
        for chunk in rows.chunks(CHUNK) {
            self.run(
                &format!(
                    "{} UNWIND $rows AS r MERGE (u:Security:Unit {{id: r.key}}) SET u += r.props, u.first_ms = CASE WHEN u.first_ms IS NULL THEN $now ELSE u.first_ms END WITH u MATCH (a:Security:Audit {{id: $audit}}) MERGE (u)-[e:OF]->(a) SET e.first_ms = CASE WHEN e.first_ms IS NULL THEN $now ELSE e.first_ms END, e.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "rows" => chunk, "audit" => audit_id, "now" => now },
            )
            .await?;
        }
        Ok(())
    }

    /// Create or update hits by id. A hit already stored keeps its place, text and host signal; only
    /// its settlement changes. A hit with a host signal is never settled `safe` or `hardening`, and
    /// `blocked` only with a cited place.
    pub async fn hits_put(&self, audit_id: &str, hits: Vec<SecurityHit>) -> Result<(), RpcError> {
        if hits.is_empty() {
            return Ok(());
        }
        self.audit_exists(audit_id).await?;
        let keys: Vec<String> = hits.iter().map(|hit| format!("{audit_id}#{}", hit.id)).collect();
        let mut stored: HashMap<String, SecurityHit> = HashMap::new();
        for chunk in keys.chunks(CHUNK) {
            let rows = self
                .run(&format!("{} UNWIND $keys AS k MATCH (h:Security:Hit {{id: k}}) RETURN {}", layer::WORKSPACE_READ, columns("h", HIT_FIELDS)), params! { "keys" => chunk })
                .await?;
            for row in &rows.rows {
                let hit: SecurityHit = keyed_from(row, HIT_FIELDS, "hit", "hit")?;
                stored.insert(hit.id.clone(), hit);
            }
        }
        let now = now_ms();
        let mut rows = Vec::with_capacity(hits.len());
        let mut fresh: Vec<(String, String, u32)> = Vec::new();
        for (hit, key) in hits.into_iter().zip(keys) {
            if hit.id.is_empty() || hit.unit.is_empty() {
                return Err(RpcError::invalid_params("a hit needs an id and its unit"));
            }
            let known = stored.get(&hit.id);
            let signal = known.and_then(|k| k.signal.clone()).or_else(|| hit.signal.clone());
            if let (Some(signal), Some(verdict)) = (signal.as_deref(), hit.verdict.as_deref()) {
                if matches!(verdict, "safe" | "hardening") {
                    return Err(RpcError::invalid_params(format!(
                        "{} is a strong host signal ({signal}): it is settled finding or lead, not {verdict}. Next: settle {} finding",
                        hit.id, hit.id
                    )));
                }
                if verdict == "blocked" && hit.cited.as_deref().is_none_or(str::is_empty) {
                    return Err(RpcError::invalid_params(format!(
                        "{} is a strong host signal ({signal}): blocked needs the cited path:line that shows a test fixture or placeholder. Next: settle {} finding",
                        hit.id, hit.id
                    )));
                }
            }
            let mut props = match known {
                Some(known) => {
                    // The place, text and signal stay as the audit first wrote them.
                    let mut props = to_props(known, "hit")?;
                    let incoming = to_props(&hit, "hit")?;
                    for field in HIT_SETTLED_FIELDS {
                        props.insert((*field).to_owned(), incoming.get(*field).cloned().unwrap_or(Value::Null));
                    }
                    props
                }
                None => {
                    fresh.push((key.clone(), hit.path.clone(), hit.line));
                    to_props(&hit, "hit")?
                }
            };
            props.remove("id");
            props.insert("hit".into(), json!(hit.id));
            props.insert("audit_id".into(), json!(audit_id));
            props.insert("last_ms".into(), json!(now));
            rows.push(json!({"key": key, "unit": format!("{audit_id}#{}", hit.unit.to_ascii_uppercase()), "props": props}));
        }
        for chunk in rows.chunks(CHUNK) {
            self.run(
                &format!(
                    "{} UNWIND $rows AS r MERGE (h:Security:Hit {{id: r.key}}) SET h += r.props, h.first_ms = CASE WHEN h.first_ms IS NULL THEN $now ELSE h.first_ms END WITH h, r MATCH (u:Security:Unit {{id: r.unit}}) MERGE (h)-[e:IN]->(u) SET e.first_ms = CASE WHEN e.first_ms IS NULL THEN $now ELSE e.first_ms END, e.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "rows" => chunk, "now" => now },
            )
            .await?;
        }
        self.link_hits(&fresh, now).await
    }

    /// `AT` edges from new hits to the code at their lines (one read per file).
    async fn link_hits(&self, hits: &[(String, String, u32)], now: u64) -> Result<(), RpcError> {
        let paths: Vec<String> = hits.iter().map(|(_, path, _)| path.trim_start_matches("./").to_owned()).filter(|p| !p.is_empty()).collect::<HashSet<_>>().into_iter().collect();
        if paths.is_empty() {
            return Ok(());
        }
        let mut code: HashMap<String, Vec<(String, String, u64, u64)>> = HashMap::new();
        for chunk in paths.chunks(CHUNK) {
            let rows = self
                .run(
                    &format!("{} UNWIND $paths AS p MATCH (n:Code {{path: p}}) WHERE (n:Sym OR n:File) AND n.ended_ms IS NULL RETURN n.path, n.sid, n.kind, n.start_line, n.end_line", layer::OBSERVED_READ),
                    params! { "paths" => chunk },
                )
                .await?;
            let number = |value: Option<&Value>| value.and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0);
            for row in &rows.rows {
                let path = row.first().and_then(Value::as_str).unwrap_or("").to_owned();
                let sid = row.get(1).and_then(Value::as_str).unwrap_or("").to_owned();
                let kind = row.get(2).and_then(Value::as_str).unwrap_or("").to_owned();
                code.entry(path).or_default().push((sid, kind, number(row.get(3)), number(row.get(4))));
            }
        }
        let mut edges = Vec::new();
        for (key, path, line) in hits {
            let Some(nodes) = code.get(path.trim_start_matches("./")) else { continue };
            if let Some(target) = innermost(nodes, *line) {
                edges.push(json!({"key": key, "sid": target}));
            }
        }
        for chunk in edges.chunks(CHUNK) {
            self.run(
                &format!(
                    "{} UNWIND $rows AS r MATCH (h:Security:Hit {{id: r.key}}) MATCH (c:Code {{sid: r.sid}}) WHERE c.ended_ms IS NULL MERGE (h)-[e:AT]->(c) SET e.first_ms = CASE WHEN e.first_ms IS NULL THEN $now ELSE e.first_ms END, e.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "rows" => chunk, "now" => now },
            )
            .await?;
        }
        Ok(())
    }

    /// Replace an audit's advisories: the ones given are written, the ones it had and no longer
    /// reports are retired.
    pub async fn advisories_put(&self, audit_id: &str, advisories: &[SecurityAdvisory]) -> Result<(), RpcError> {
        self.audit_exists(audit_id).await?;
        let now = now_ms();
        let mut rows = Vec::with_capacity(advisories.len());
        let mut keys = Vec::with_capacity(advisories.len());
        for advisory in advisories {
            if advisory.package.is_empty() || advisory.advisory.is_empty() {
                return Err(RpcError::invalid_params("an advisory needs its package and its id"));
            }
            let key = format!("{audit_id}#{}:{}:{}:{}", advisory.ecosystem, advisory.package, advisory.version, advisory.advisory);
            let mut props = to_props(advisory, "advisory")?;
            props.insert("audit_id".into(), json!(audit_id));
            props.insert("ended_ms".into(), Value::Null);
            props.insert("ended_reason".into(), Value::Null);
            props.insert("state".into(), Value::Null);
            props.insert("last_ms".into(), json!(now));
            rows.push(json!({"key": key, "dep": crate::model::dep_sid(&advisory.ecosystem, &advisory.package), "props": props}));
            keys.push(key);
        }
        self.run(
            &format!("{} MATCH (v:Security:Advisory {{audit_id: $audit}}) WHERE v.ended_ms IS NULL AND NOT v.id IN $keys SET v.ended_ms = $now, v.ended_reason = 'no longer reported', v.state = 'gone', v.last_ms = $now", layer::WORKSPACE_WRITE),
            params! { "audit" => audit_id, "keys" => keys, "now" => now },
        )
        .await?;
        for chunk in rows.chunks(CHUNK) {
            self.run(
                &format!(
                    "{} UNWIND $rows AS r MERGE (v:Security:Advisory {{id: r.key}}) SET v += r.props, v.first_ms = CASE WHEN v.first_ms IS NULL THEN $now ELSE v.first_ms END WITH v, r MATCH (a:Security:Audit {{id: $audit}}) MERGE (v)-[e:OF]->(a) SET e.first_ms = CASE WHEN e.first_ms IS NULL THEN $now ELSE e.first_ms END, e.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "rows" => chunk, "audit" => audit_id, "now" => now },
            )
            .await?;
            self.run(
                &format!("{} UNWIND $rows AS r MATCH (v:Security:Advisory {{id: r.key}}) MATCH (d:Code:Dependency {{sid: r.dep}}) WHERE d.ended_ms IS NULL MERGE (v)-[e:IN_PACKAGE]->(d) SET e.first_ms = CASE WHEN e.first_ms IS NULL THEN $now ELSE e.first_ms END, e.last_ms = $now", layer::WORKSPACE_WRITE),
                params! { "rows" => chunk, "now" => now },
            )
            .await?;
        }
        Ok(())
    }

    // ---- findings ------------------------------------------------------------------------------

    async fn finding_row(&self, id: &str) -> Result<Option<Vec<Value>>, RpcError> {
        let rows = self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}}) RETURN {}", layer::WORKSPACE_READ, columns("f", FINDING_FIELDS)), params! { "id" => id }).await?;
        Ok(rows.rows.into_iter().next())
    }

    /// A finding with its history (with the age term); `None` when there is none or it was forgotten.
    pub async fn finding(&self, id: &str) -> Result<Option<SecurityFindingDetail>, RpcError> {
        let Some((finding, history)) = self.load(id).await? else { return Ok(None) };
        Ok(Some(SecurityFindingDetail { finding: with_age(finding, now_ms()), history }))
    }

    /// A stored finding (not forgotten) and its history.
    async fn load(&self, id: &str) -> Result<Option<(SecurityFinding, Vec<SecurityStatusChange>)>, RpcError> {
        let Some(row) = self.finding_row(id).await? else { return Ok(None) };
        let parts = self.parts(&[id.to_owned()]).await?;
        let parts = parts.get(id);
        let finding = finding_from(&row, parts)?;
        if finding.ended_ms.is_some() {
            return Ok(None);
        }
        Ok(Some((finding, history_from(&row, parts))))
    }

    /// Write a finding's node and its edges; `history` is its whole lifecycle so far.
    async fn write(&self, finding: &SecurityFinding, history: &[SecurityStatusChange], now: u64) -> Result<(), RpcError> {
        let mut props = to_props(finding, "finding")?;
        // The trace, quick fix and history are nodes of their own (an earlier version's text goes).
        props.insert("trace".into(), Value::Null);
        props.insert("quick_fix".into(), Value::Null);
        props.insert("history".into(), Value::Null);
        props.insert("project".into(), json!(self.scope));
        props.insert("name".into(), json!(finding.title));
        props.insert("updated_ms".into(), json!(now));
        props.insert("last_ms".into(), json!(now));
        // A forgotten finding is retired; one written live has no retirement.
        let forgotten = finding.ended_ms.is_some();
        props.insert("ended_reason".into(), if forgotten { json!("forgotten by the person") } else { Value::Null });
        props.insert("state".into(), if forgotten { json!("gone") } else { Value::Null });
        self.run(
            &format!("{} MERGE (f:Security:Finding {{id: $id}}) SET f += $props, f.first_ms = CASE WHEN f.first_ms IS NULL THEN $now ELSE f.first_ms END", layer::WORKSPACE_WRITE),
            params! { "id" => finding.id, "props" => props, "now" => now },
        )
        .await?;
        self.indexes().await?;
        self.link_root("MATCH (w:Security:Finding {id: $id})", params! { "id" => finding.id }).await?;
        if let Some(audit) = &finding.audit_id {
            self.run(
                &format!(
                    "{} MATCH (f:Security:Finding {{id: $id}}), (a:Security:Audit {{id: $audit}}) MERGE (f)-[e:FOUND_BY]->(a) SET e.first_ms = CASE WHEN e.first_ms IS NULL THEN $now ELSE e.first_ms END, e.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "id" => finding.id, "audit" => audit, "now" => now },
            )
            .await?;
        }
        self.link_trace(finding, now).await?;
        self.write_parts(finding, history, now).await?;
        self.end_edges(&finding.id, "IN_PACKAGE", now).await?;
        let package = finding.quick_fix.as_ref().map(|fix| (fix.ecosystem.clone(), fix.package.clone())).filter(|(_, package)| !package.is_empty());
        if let Some((ecosystem, package)) = package {
            self.run(
                &format!(
                    "{} MATCH (f:Security:Finding {{id: $id}}) MATCH (d:Code:Dependency {{sid: $sid}}) WHERE d.ended_ms IS NULL MERGE (f)-[e:IN_PACKAGE]->(d) SET e.ended_ms = null, e.first_ms = CASE WHEN e.first_ms IS NULL THEN $now ELSE e.first_ms END, e.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "id" => finding.id, "sid" => crate::model::dep_sid(&ecosystem, &package), "now" => now },
            )
            .await?;
        }
        self.link_session(&finding.id, finding.session_id.as_deref(), now).await
    }

    /// The parts of findings: their trace steps, quick fix and status moves, by finding id.
    async fn parts(&self, ids: &[String]) -> Result<HashMap<String, Parts>, RpcError> {
        let mut out: HashMap<String, Parts> = HashMap::new();
        let rows = self.run(&format!("{} UNWIND $ids AS i MATCH (f:Security:Finding {{id: i}})-[:HAS_STEP]->(t:SecurityTraceStep) WHERE t.ended_ms IS NULL RETURN i, t", layer::WORKSPACE_READ), params! { "ids" => ids }).await?;
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
        let rows = self.run(&format!("{} UNWIND $ids AS i MATCH (f:Security:Finding {{id: i}})-[:QUICK_FIX]->(q:SecurityQuickFix) WHERE q.ended_ms IS NULL RETURN i, q", layer::WORKSPACE_READ), params! { "ids" => ids }).await?;
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

    /// Write a finding's trace steps and quick fix (each by its place; the stored ones the new
    /// report no longer has are retired) and the status moves not stored yet.
    async fn write_parts(&self, finding: &SecurityFinding, history: &[SecurityStatusChange], now: u64) -> Result<(), RpcError> {
        let id = &finding.id;
        let live = |var: &str| format!("{var}.ended_ms = null, {var}.ended_reason = null, {var}.state = null, {var}.first_ms = CASE WHEN {var}.first_ms IS NULL THEN $now ELSE {var}.first_ms END, {var}.last_ms = $now");
        self.run(
            &format!(
                "{} MATCH (f:Security:Finding {{id: $id}})-[e:HAS_STEP]->(t:SecurityTraceStep) WHERE t.ended_ms IS NULL AND t.position >= $count SET t.ended_ms = $now, t.ended_reason = 'replaced', t.state = 'gone', t.last_ms = $now, e.ended_ms = $now, e.last_ms = $now",
                layer::WORKSPACE_WRITE
            ),
            params! { "id" => id, "count" => finding.trace.len(), "now" => now },
        )
        .await?;
        for (position, step) in finding.trace.iter().enumerate() {
            self.run(
                &format!(
                    "{} MATCH (f:Security:Finding {{id: $finding}}) MERGE (t:SecurityTraceStep {{id: $id}}) SET t.finding_id = $finding, t.position = $position, t.kind = $kind, t.file = $file, t.line = $line, t.what = $what, {} MERGE (f)-[e:HAS_STEP]->(t) SET {}",
                    layer::WORKSPACE_WRITE,
                    live("t"),
                    live("e")
                ),
                params! { "finding" => id, "id" => format!("{id}#{position}"), "position" => position, "kind" => step.kind, "file" => step.file, "line" => step.line, "what" => step.what, "now" => now },
            )
            .await?;
        }
        match &finding.quick_fix {
            Some(fix) => {
                self.run(
                    &format!(
                        "{} MATCH (f:Security:Finding {{id: $finding}}) MERGE (q:SecurityQuickFix {{id: $finding}}) SET q.finding_id = $finding, q.kind = $kind, q.ecosystem = $ecosystem, q.package = $package, q.from = $from, q.to = $to, q.lockfile = $lockfile, q.commands = $commands, {} MERGE (f)-[e:QUICK_FIX]->(q) SET {}",
                        layer::WORKSPACE_WRITE,
                        live("q"),
                        live("e")
                    ),
                    params! { "finding" => id, "kind" => fix.kind, "ecosystem" => fix.ecosystem, "package" => fix.package, "from" => fix.from, "to" => fix.to, "lockfile" => fix.lockfile, "commands" => fix.commands, "now" => now },
                )
                .await?;
            }
            None => {
                self.run(
                    &format!(
                        "{} MATCH (f:Security:Finding {{id: $id}})-[e:QUICK_FIX]->(q:SecurityQuickFix) WHERE q.ended_ms IS NULL SET q.ended_ms = $now, q.ended_reason = 'replaced', q.state = 'gone', q.last_ms = $now, e.ended_ms = $now, e.last_ms = $now",
                        layer::WORKSPACE_WRITE
                    ),
                    params! { "id" => id, "now" => now },
                )
                .await?;
            }
        }
        let stored = self.run(&format!("{} MATCH (f:Security:Finding {{id: $id}})-[:CHANGED]->(c:SecurityStatusChange) RETURN count(c)", layer::WORKSPACE_READ), params! { "id" => id }).await?;
        let known = stored.rows.first().and_then(|row| row.first()).and_then(Value::as_u64).unwrap_or(0) as usize;
        for (position, change) in history.iter().enumerate().skip(known) {
            self.run(
                &format!(
                    "{} MATCH (f:Security:Finding {{id: $finding}}) MERGE (c:SecurityStatusChange {{id: $id}}) SET c.finding_id = $finding, c.position = $position, c.status = $status, c.note = $note, c.by = $by, c.ms = $ms, c.session_id = $session, c.commit = $commit, c.first_ms = $ms, c.last_ms = $ms MERGE (f)-[e:CHANGED]->(c) SET e.first_ms = CASE WHEN e.first_ms IS NULL THEN $ms ELSE e.first_ms END, e.last_ms = $ms",
                    layer::WORKSPACE_WRITE
                ),
                params! {
                    "finding" => id, "id" => format!("{id}#{position}"), "position" => position, "status" => change.status, "note" => change.note, "by" => change.by,
                    "ms" => change.ms, "session" => change.session_id, "commit" => change.commit
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Replace a finding's `AT` edges: one per trace step whose file and line are live indexed
    /// code. The ones in force end; a step's edge to the same code is opened again.
    async fn link_trace(&self, finding: &SecurityFinding, now: u64) -> Result<(), RpcError> {
        self.end_edges(&finding.id, "AT", now).await?;
        for (step, place) in finding.trace.iter().enumerate() {
            let Some(target) = self.code_at(&place.file, place.line).await? else { continue };
            self.run(
                &format!(
                    "{} MATCH (f:Security:Finding {{id: $id}}) MATCH (c:Code {{sid: $target}}) WHERE c.ended_ms IS NULL MERGE (f)-[r:AT {{step: $step}}]->(c) SET r.kind = $kind, r.what = $what, r.line = $line, r.ended_ms = null, r.first_ms = CASE WHEN r.first_ms IS NULL THEN $now ELSE r.first_ms END, r.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "id" => finding.id, "target" => target, "step" => step, "kind" => place.kind, "what" => place.what, "line" => place.line, "now" => now },
            )
            .await?;
        }
        Ok(())
    }

    /// The sid of the innermost symbol whose range covers `line` in `path`, else the file's.
    async fn code_at(&self, path: &str, line: u32) -> Result<Option<String>, RpcError> {
        let path = path.trim_start_matches("./");
        let rows = self.run(&format!("{} MATCH (n:Code {{path: $p}}) WHERE (n:Sym OR n:File) AND n.ended_ms IS NULL RETURN n.sid, n.kind, n.start_line, n.end_line", layer::OBSERVED_READ), params! { "p" => path }).await?;
        let number = |value: Option<&Value>| value.and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0);
        let nodes: Vec<(String, String, u64, u64)> = rows
            .rows
            .iter()
            .map(|row| (row.first().and_then(|v| v.as_str()).unwrap_or("").to_owned(), row.get(1).and_then(|v| v.as_str()).unwrap_or("").to_owned(), number(row.get(2)), number(row.get(3))))
            .collect();
        Ok(innermost(&nodes, line))
    }

    /// Replace a finding's `FIXED_IN` edge with one to the session, when the session is a node
    /// here (the edge in force ends; the same session's is opened again).
    async fn link_session(&self, id: &str, session: Option<&str>, now: u64) -> Result<(), RpcError> {
        self.end_edges(id, "FIXED_IN", now).await?;
        if let Some(session) = session.filter(|session| !session.is_empty()) {
            self.run(
                &format!(
                    "{} MATCH (f:Security:Finding {{id: $id}}), (s:Session {{id: $session}}) MERGE (f)-[r:FIXED_IN]->(s) SET r.ended_ms = null, r.first_ms = CASE WHEN r.first_ms IS NULL THEN $now ELSE r.first_ms END, r.last_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "id" => id, "session" => session, "now" => now },
            )
            .await?;
        }
        Ok(())
    }

    /// Create or update findings by id. A new finding is open (a rejected claim is gone); a known
    /// one keeps its lifecycle and takes the new content; one that was fixed, verified or gone (by
    /// the model) is open again; one the person closed or forgot stays so. A host-set severity is a
    /// floor. Each finding's evidence is read from the working tree `root`.
    pub async fn findings_put(&self, audit_id: Option<&str>, findings: Vec<SecurityFinding>, root: &Path) -> Result<PutOutcome, RpcError> {
        let now = now_ms();
        let audit = audit_id.map(str::to_owned);
        let audit_name = audit.clone().unwrap_or_else(|| "watch".to_owned());
        let mut outcome = PutOutcome::default();
        for mut finding in findings {
            if finding.id.is_empty() {
                return Err(RpcError::invalid_params("a finding needs an id (its fingerprint)"));
            }
            if finding.source.is_empty() {
                finding.source = "audit".into();
            }
            if let Some(sink) = finding.trace.last() {
                if finding.sink_path.as_deref().is_none_or(str::is_empty) {
                    finding.sink_path = Some(sink.file.clone());
                }
                if finding.sink_line.is_none_or(|line| line == 0) && sink.line > 0 {
                    finding.sink_line = Some(sink.line);
                }
            }
            record_evidence(root, &mut finding);
            let mut history: Vec<SecurityStatusChange>;
            let row = self.finding_row(&finding.id).await?;
            match row {
                None => {
                    self.floor_severity(&mut finding, None, &mut outcome.notes);
                    let rejected = finding.verdict == "rejected";
                    let note = if rejected {
                        format!("rejected by audit {audit_name}: {}", finding.reason.clone().unwrap_or_default())
                    } else {
                        format!("found by {} {audit_name}", if audit.is_some() { "audit" } else { "the" })
                    };
                    finding.status = if rejected { "gone".into() } else { "open".into() };
                    finding.status_by = "model".into();
                    finding.status_note = note.clone();
                    finding.status_ms = now;
                    finding.first_seen_ms = now;
                    finding.seen_in = 1;
                    finding.session_id = None;
                    finding.fixed_commit = None;
                    finding.notified_ms = None;
                    finding.ended_ms = None;
                    history = vec![SecurityStatusChange { status: finding.status.clone(), by: "model".into(), ms: now, note, session_id: None, commit: None }];
                }
                Some(row) => {
                    let parts = self.parts(std::slice::from_ref(&finding.id)).await?;
                    let parts = parts.get(&finding.id);
                    let known = finding_from(&row, parts)?;
                    history = history_from(&row, parts);
                    if known.ended_ms.is_some() {
                        outcome.notes.push(format!("{} was forgotten by the person; it stays out of the list", known.id));
                        outcome.findings.push(known);
                        continue;
                    }
                    if finding.verdict == "rejected" && known.verdict != "rejected" {
                        outcome.notes.push(format!("{} is kept as {} ({}): a person closes it in the Security tab", known.id, known.verdict.replace('_', " "), known.status));
                        outcome.findings.push(known);
                        continue;
                    }
                    self.floor_severity(&mut finding, Some(&known), &mut outcome.notes);
                    finding.status = known.status;
                    finding.status_note = known.status_note;
                    finding.status_by = known.status_by;
                    finding.status_ms = known.status_ms;
                    finding.first_seen_ms = known.first_seen_ms;
                    finding.seen_in = known.seen_in + 1;
                    finding.session_id = known.session_id;
                    finding.fixed_commit = known.fixed_commit;
                    finding.notified_ms = known.notified_ms;
                    finding.ended_ms = None;
                    let person_closed = finding.status == "gone" && finding.status_by == "user";
                    if person_closed {
                        outcome.notes.push(format!("{} stays gone: the person marked it not an issue", finding.id));
                    } else if finding.verdict != "rejected" && matches!(finding.status.as_str(), "fixed" | "verified" | "gone") {
                        // Reported again after it was closed: the fix did not hold, or the code came back.
                        let change = SecurityStatusChange { status: "open".into(), by: "recheck".into(), ms: now, note: format!("reported again by {audit_name}"), session_id: None, commit: None };
                        finding.status = change.status.clone();
                        finding.status_by = change.by.clone();
                        finding.status_note = change.note.clone();
                        finding.status_ms = now;
                        history.push(change);
                    }
                }
            }
            finding.last_seen_ms = now;
            if audit.is_some() {
                finding.audit_id = audit.clone();
            }
            let urgent = severity_rank(finding.severity_overall.as_deref()) >= 3 || finding.kev;
            if finding.source == "watch" && urgent && finding.notified_ms.is_none() && finding.status == "open" {
                finding.notified_ms = Some(now);
                outcome.notify.push(finding.clone());
            }
            self.write(&finding, &history, now).await?;
            outcome.findings.push(finding);
        }
        Ok(outcome)
    }

    /// Keep a finding's severity at or above the host's: the larger of the stored and the given floor.
    fn floor_severity(&self, finding: &mut SecurityFinding, known: Option<&SecurityFinding>, notes: &mut Vec<String>) {
        let floor = [known.and_then(|k| k.host_severity.clone()), finding.host_severity.clone()].into_iter().flatten().max_by_key(|s| severity_rank(Some(s.as_str())));
        let Some(floor) = floor else { return };
        finding.host_severity = Some(floor.clone());
        if severity_rank(finding.severity_overall.as_deref()) < severity_rank(Some(floor.as_str())) {
            if finding.severity_overall.is_some() {
                notes.push(format!("{}: severity kept at {floor}, the host's for {}; it can be raised, not lowered", finding.id, finding.signal.clone().unwrap_or_else(|| "this signal".into())));
            }
            finding.severity_overall = Some(floor.clone());
        }
        if finding.severity_impact.is_some() && severity_rank(finding.severity_impact.as_deref()) < severity_rank(Some(floor.as_str())) {
            finding.severity_impact = Some(floor);
        }
    }

    /// The project's findings in the asked statuses (active ones by default; every status when
    /// asked by ids, audit or session), not forgotten, highest priority (with the age term) first,
    /// the oldest first among equals.
    pub async fn findings(&self, p: &SecurityFindingsParams) -> Result<Vec<SecurityFinding>, RpcError> {
        let statuses: Vec<String> = match &p.status {
            Some(list) if !list.is_empty() => list.clone(),
            _ if p.ids.is_some() || p.audit_id.is_some() || p.session_id.is_some() => STATUSES.iter().map(|s| (*s).to_owned()).collect(),
            _ => ACTIVE.iter().map(|s| (*s).to_owned()).collect(),
        };
        let rows = self
            .run(
                &format!("{} UNWIND $statuses AS st MATCH (f:Security:Finding {{status: st}}) WHERE f.ended_ms IS NULL RETURN {}", layer::WORKSPACE_READ, columns("f", FINDING_FIELDS)),
                params! { "statuses" => statuses },
            )
            .await?;
        let wanted: Option<HashSet<&String>> = p.ids.as_ref().map(|ids| ids.iter().collect());
        let keep = |row: &&Vec<Value>| -> bool {
            let id = row.first().and_then(Value::as_str).unwrap_or("").to_owned();
            wanted.as_ref().is_none_or(|set| set.contains(&id))
        };
        let rows: Vec<&Vec<Value>> = rows.rows.iter().filter(keep).collect();
        let ids: Vec<String> = rows.iter().filter_map(|row| row.first().and_then(Value::as_str).map(str::to_owned)).collect();
        let parts = self.parts(&ids).await?;
        let now = now_ms();
        let mut findings = rows
            .iter()
            .map(|row| finding_from(row, row.first().and_then(Value::as_str).and_then(|id| parts.get(id))))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|f| with_age(f, now))
            .filter(|f| p.class.as_ref().is_none_or(|class| &f.class == class))
            .filter(|f| p.audit_id.as_ref().is_none_or(|audit| f.audit_id.as_ref() == Some(audit)))
            .filter(|f| p.session_id.as_ref().is_none_or(|session| f.session_id.as_ref() == Some(session)))
            .filter(|f| p.source.as_ref().is_none_or(|source| &f.source == source || (source == "audit" && f.source.is_empty())))
            .filter(|f| p.min_priority.is_none_or(|min| f.priority >= min))
            .collect::<Vec<_>>();
        findings.sort_by(|a, b| (std::cmp::Reverse(a.priority), a.first_seen_ms, &a.id).cmp(&(std::cmp::Reverse(b.priority), b.first_seen_ms, &b.id)));
        Ok(findings)
    }

    /// Apply one status move to a finding and write it.
    async fn apply_move(&self, mut finding: SecurityFinding, mut history: Vec<SecurityStatusChange>, change: SecurityStatusChange) -> Result<SecurityFinding, RpcError> {
        let now = change.ms;
        finding.status = change.status.clone();
        finding.status_note = change.note.clone();
        finding.status_by = change.by.clone();
        finding.status_ms = now;
        if let Some(session) = change.session_id.clone().filter(|s| !s.is_empty()) {
            finding.session_id = Some(session);
        }
        if let Some(commit) = change.commit.clone().filter(|c| !c.is_empty()) {
            finding.fixed_commit = Some(commit);
        }
        history.push(change);
        self.write(&finding, &history, now).await?;
        Ok(finding)
    }

    /// A lifecycle move, refused when the lifecycle does not allow it, when it is the person's and
    /// a command asks, or when a `fixed` is not borne out by the working tree `root`.
    pub async fn finding_update(&self, p: SecurityFindingUpdateParams, caller: &Caller, root: &Path) -> Result<Option<SecurityFinding>, RpcError> {
        let Some((finding, history)) = self.load(&p.id).await? else { return Ok(None) };
        let current = finding.status.clone();
        let id = p.id.clone();
        let mut by = p.by.clone();
        let mut session = p.session_id.clone().filter(|s| !s.is_empty());
        if let Some(command) = caller.command_session.as_deref() {
            let person = |what: &str| RpcError::invalid_params(format!("finding {id} is {current}; {what} is the person's decision: the person decides this in the Security tab."));
            match p.status.as_str() {
                "accepted" => return Err(person("accepting the risk")),
                "gone" => return Err(person("marking it not an issue")),
                "verified" => {
                    return Err(RpcError::invalid_params(format!(
                        "finding {id} is {current}; verified comes from the host's re-check after the fixing session. Next: reply with what was changed and how it was verified."
                    )));
                }
                "open" if !(current == "fixing" && finding.session_id.as_deref() == Some(command)) => return Err(person("reopening it")),
                "fixing" => session = Some(command.to_owned()),
                _ => {}
            }
            by = if p.by == "recheck" { "recheck".into() } else { "agent".into() };
        }
        let moves = allowed_moves(&current);
        let fixed_by_person = current == "open" && p.status == "fixed" && by == "user";
        if !moves.contains(&p.status.as_str()) || fixed_by_person {
            let allowed: Vec<String> = moves.iter().map(|m| if current == "open" && *m == "fixed" { "fixed (once the host sees the sink gone)".to_owned() } else { (*m).to_owned() }).collect();
            return Err(RpcError::invalid_params(format!("finding {id} is {current}; it can move to {}, not {}", allowed.join(", "), p.status)));
        }
        if p.status == "fixing" && session.is_none() {
            return Err(RpcError::invalid_params(format!("finding {id} moves to fixing with the session that fixes it: pass session_id")));
        }
        let mut note = p.note.clone();
        if p.status == "fixed" && by != "user" {
            match evidence_now(root, &finding) {
                Evidence::Gone(sentence) => note = if note.is_empty() { sentence } else { format!("{note} ({sentence})") },
                Evidence::At { sentence, .. } => {
                    return Err(RpcError::invalid_params(format!("finding {id} is not fixed: {sentence}. Next: change the sink so this line is gone, run the test, then record the fix again.")));
                }
                Evidence::Unknown(sentence) => {
                    return Err(RpcError::invalid_params(format!("finding {id} cannot be checked: {sentence}. Next: run the audit again (secaudit.py next) so the finding carries its sink, then record the fix.")));
                }
            }
            if current == "open" && caller.command() {
                by = "recheck".into();
            }
        }
        let change = SecurityStatusChange { status: p.status, note, by, ms: now_ms(), session_id: session, commit: p.commit };
        Ok(Some(self.apply_move(finding, history, change).await?))
    }

    /// The host's re-check: what the working tree shows of each finding's sink, and the moves it
    /// makes (see [`methods::SECURITY_RECHECK`](cc_protocol::contracts::memory::methods::SECURITY_RECHECK)).
    pub async fn recheck(&self, p: &SecurityRecheckParams, caller: &Caller, root: &Path) -> Result<(Vec<SecurityRecheck>, Vec<String>), RpcError> {
        let statuses: Vec<String> = if p.ids.is_some() { STATUSES.iter().map(|s| (*s).to_owned()).collect() } else { ["open", "fixing", "fixed"].iter().map(|s| (*s).to_owned()).collect() };
        let list = self.findings(&SecurityFindingsParams { project_id: self.scope.clone(), status: Some(statuses), ids: p.ids.clone(), ..Default::default() }).await?;
        let mut checked = Vec::with_capacity(list.len());
        let mut moved = Vec::new();
        for finding in list {
            let evidence = evidence_now(root, &finding);
            let mut row = SecurityRecheck {
                id: finding.id.clone(),
                title: finding.title.clone(),
                kind: evidence.kind().to_owned(),
                line: evidence.line(),
                sentence: evidence.sentence().to_owned(),
                was: finding.status.clone(),
                now: None,
            };
            let fixer_here = caller.command_session.is_some() && caller.command_session == finding.session_id;
            let next = match (finding.status.as_str(), &evidence) {
                ("open" | "fixing", Evidence::Gone(sentence)) => Some(("fixed", sentence.clone())),
                ("fixed", Evidence::Gone(_)) if !fixer_here => Some(("verified", "a later re-check still finds the sink gone".to_owned())),
                ("fixed", Evidence::At { sentence, .. }) => Some(("open", format!("back: {sentence}"))),
                _ => None,
            };
            if let Some((status, note)) = next
                && !p.dry
            {
                // A finding gone since the list was read is still answered, unmoved.
                let Some((stored, history)) = self.load(&finding.id).await? else {
                    checked.push(row);
                    continue;
                };
                let change = SecurityStatusChange { status: status.to_owned(), note, by: "recheck".into(), ms: now_ms(), session_id: None, commit: None };
                let after = self.apply_move(stored, history, change).await?;
                row.now = Some(after.status.clone());
                moved.push(after.id);
            }
            checked.push(row);
        }
        Ok((checked, moved))
    }

    /// A session ended: the findings it was fixing and did not fix are open again.
    pub async fn session_ended(&self, session_id: &str) -> Result<Vec<String>, RpcError> {
        let list = self
            .findings(&SecurityFindingsParams { project_id: self.scope.clone(), status: Some(vec!["fixing".into()]), session_id: Some(session_id.to_owned()), ..Default::default() })
            .await?;
        let mut reopened = Vec::new();
        for finding in list {
            let Some((stored, history)) = self.load(&finding.id).await? else { continue };
            let change = SecurityStatusChange {
                status: "open".into(),
                note: "the fixing session ended without a recorded fix".into(),
                by: "host".into(),
                ms: now_ms(),
                session_id: Some(session_id.to_owned()),
                commit: None,
            };
            reopened.push(self.apply_move(stored, history, change).await?.id);
        }
        Ok(reopened)
    }

    pub async fn summary(&self) -> Result<SecuritySummary, RpcError> {
        let rows = self.run(&format!("{} MATCH (f:Security:Finding) WHERE f.ended_ms IS NULL RETURN {}", layer::WORKSPACE_READ, columns("f", FINDING_FIELDS)), Map::new()).await?;
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
                if finding.kev {
                    summary.kev += 1;
                }
            }
        }
        summary.last_audit = self.audits().await?.into_iter().max_by_key(|audit| (audit.started_ms, audit.id.clone()));
        Ok(summary)
    }

    /// Retire a finding (the person's forget): it leaves the lists, its node, edges and history
    /// stay. Returns whether there was one.
    pub async fn forget(&self, id: &str) -> Result<bool, RpcError> {
        let Some((finding, history)) = self.load(id).await? else { return Ok(false) };
        let now = now_ms();
        let mut finding = finding;
        finding.ended_ms = Some(now);
        let mut history = history;
        history.push(SecurityStatusChange { status: finding.status.clone(), note: "forgotten by the person".into(), by: "user".into(), ms: now, session_id: None, commit: None });
        self.write(&finding, &history, now).await?;
        Ok(true)
    }
}

/// The innermost symbol whose range covers `line` among a file's nodes `(sid, kind, start, end)`,
/// else the file itself.
fn innermost(nodes: &[(String, String, u64, u64)], line: u32) -> Option<String> {
    let mut file = None;
    let mut best: Option<(u64, String)> = None;
    for (sid, kind, start, end) in nodes {
        if kind == "file" {
            file = Some(sid.clone());
            continue;
        }
        let covers = *start <= u64::from(line) && u64::from(line) <= (*end).max(*start);
        if covers && best.as_ref().is_none_or(|(span, _)| end.saturating_sub(*start) < *span) {
            best = Some((end.saturating_sub(*start), sid.clone()));
        }
    }
    best.map(|(_, sid)| sid).or(file)
}
