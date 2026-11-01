//! The deep jobs' records, in the memory plugin's graph (workspace layer), and a copy in RAM that
//! the jobs read and change.
//!
//! ```text
//! (:ResearchJob {key: job id, project_id, session_id, topic, state*, phase*, round, depth, questions,
//!     sources_read, sources_failed, notes, report_path, error, resumed, started_ms, updated_ms,
//!     param_project_id, param_topic, param_questions, param_depth, param_sources, param_since,
//!     param_max_sources, param_root, param_fresh, param_session_id,
//!     visited, queries_run, sources_searched, dropped_quotes, summary, open_questions})
//!     -[:FOR]->(:Project)  -[:IN]->(:Session)   (the session that asked, when one did)
//! (:ResearchQuestion {key: '<job>#<n>', position, question, queries, sources, pending})-[:OF]->(:ResearchJob)
//! (:ResearchSource {key: '<job>#<read url>', url, read_url, title, source, date, authors, snippet,
//!     score, questions, found_by, position, state: candidate | unread, why, extra_<name>…})-[:OF]->(:ResearchJob)
//! (:ResearchFinding {key: '<job>#<n>', position, statement, notes, confidence})-[:OF]->(:ResearchJob)
//!     -[:SUPPORTED_BY]->(:Note {id})   (one per note the finding cites)
//! * temporal: `state` and `phase` keep their history (a whole node leaves them out, so a read
//!   names them as their own columns)
//! ```
//!
//! A question keeps its node after its round is searched (`pending: false`); a new round's
//! questions get the next positions. A source's `position` is its place among the round's
//! candidates (absent when it is not one); a source that is neither a candidate nor unread any
//! more, and a finding a new synthesis no longer has, is retired. `summary` is absent until the
//! job has a synthesis.
//!
//! Every checkpoint is one put of the job node and the children that changed, written in order by
//! one writer task; while memory is away the writer waits and the jobs go on in RAM. A job's
//! hand-off to a successor (a hot swap) flushes the writer, and the successor reads the job back
//! from the graph before it resumes it.
//!
//! Jobs an earlier version kept as files (`<data>/jobs/<id>.json`) are written to the graph once
//! and the files deleted. The report each job writes into the project (`docs/research/*.md`) is
//! the user's deliverable and stays a file.

use crate::deep::{Candidate, Finding, JobState, Planned, Synthesis, Unread};
use crate::graph::{self, Graph, PATIENT, edge, gref, node};
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{GraphEdge, GraphNode, GraphRef};
use cc_protocol::contracts::research::{DeepParams, Job, SearchHit};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const JOB: &str = "ResearchJob";
const QUESTION: &str = "ResearchQuestion";
const SOURCE: &str = "ResearchSource";
const FINDING: &str = "ResearchFinding";

/// Where a job's questions stand: the position of its first pending question, and how many it
/// has had in all.
#[derive(Debug, Clone, Copy, Default)]
struct Questions {
    base: u32,
    total: u32,
}

struct Kept {
    state: JobState,
    questions: Questions,
}

/// One checkpoint's writes.
struct Batch {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    retire: Vec<GraphRef>,
    /// Findings' `SUPPORTED_BY` edges to their notes, written after the checkpoint on their own: a
    /// note that is gone fails only these, never the checkpoint.
    supports: Vec<GraphEdge>,
}

enum Write {
    Batch(Batch),
    Flush(oneshot::Sender<()>),
}

pub struct JobStore {
    graph: Arc<Graph>,
    /// Where an earlier version kept jobs as files (moved into the graph once).
    legacy: PathBuf,
    kept: Mutex<HashMap<String, Kept>>,
    writes: mpsc::UnboundedSender<Write>,
    writer: Mutex<Option<mpsc::UnboundedReceiver<Write>>>,
}

impl JobStore {
    pub fn new(graph: Arc<Graph>, legacy: PathBuf) -> Self {
        let (writes, writer) = mpsc::unbounded_channel();
        Self { graph, legacy, kept: Mutex::new(HashMap::new()), writes, writer: Mutex::new(Some(writer)) }
    }

    /// Read every job from the graph into RAM, move the files of an earlier version in, and start
    /// the writer. Fails while memory stays away; the caller tries again on its next call.
    pub async fn load(&self) -> Result<(), RpcError> {
        let rows = self.graph.query(&format!("MATCH (j:{JOB}) WHERE j.ended_ms IS NULL RETURN j, j.state, j.phase"), json!({}), 5000, PATIENT).await?;
        let mut loaded = HashMap::new();
        for row in rows {
            let props = job_row(&row);
            let id = graph::text(&props, "key");
            if id.is_empty() {
                continue;
            }
            let kept = self.read_children(&id, props).await?;
            loaded.insert(id, kept);
        }
        self.keep_history(&loaded).await;
        for state in self.migrate(&loaded).await {
            let questions = Questions { base: 0, total: state.pending.len() as u32 };
            loaded.insert(state.job.job_id.clone(), Kept { state, questions });
        }
        if let Ok(mut kept) = self.kept.lock() {
            *kept = loaded;
        }
        if let Some(receiver) = self.writer.lock().ok().and_then(|mut w| w.take()) {
            tokio::spawn(write_loop(self.graph.clone(), receiver));
        }
        Ok(())
    }

    /// Read one job back from the graph (a predecessor may have written it since the load).
    pub async fn reload(&self, job_id: &str) -> Result<Option<JobState>, RpcError> {
        let rows = self.graph.query(&format!("MATCH (j:{JOB}) WHERE j.key = $job AND j.ended_ms IS NULL RETURN j, j.state, j.phase"), json!({ "job": job_id }), 1, PATIENT).await?;
        let Some(row) = rows.first() else { return Ok(None) };
        let kept = self.read_children(job_id, job_row(row)).await?;
        let state = kept.state.clone();
        if let Ok(mut all) = self.kept.lock() {
            all.insert(job_id.to_owned(), kept);
        }
        Ok(Some(state))
    }

    async fn read_children(&self, job_id: &str, job: Map<String, Value>) -> Result<Kept, RpcError> {
        let rows = self
            .graph
            .query(&format!("MATCH (n)-[:OF]->(j:{JOB}) WHERE j.key = $job AND n.ended_ms IS NULL RETURN n"), json!({ "job": job_id }), 5000, PATIENT)
            .await?;
        let mut questions: Vec<(u32, bool, Planned)> = Vec::new();
        let mut candidates: Vec<(u64, Candidate)> = Vec::new();
        let mut unread: Vec<(u64, String, Unread)> = Vec::new();
        let mut findings: Vec<(u64, Finding)> = Vec::new();
        for row in rows {
            let cell = row.first().cloned().unwrap_or(Value::Null);
            let labels = graph::labels_of(&cell);
            let p = graph::props_of(&cell);
            if labels.iter().any(|l| l == QUESTION) {
                let planned = Planned { question: graph::text(&p, "question"), queries: graph::texts(&p, "queries"), sources: graph::texts(&p, "sources") };
                questions.push((graph::number(&p, "position") as u32, graph::flag(&p, "pending"), planned));
            } else if labels.iter().any(|l| l == SOURCE) {
                if let Some(position) = graph::opt_number(&p, "position") {
                    candidates.push((position, candidate_from(&p)));
                }
                if graph::text(&p, "state") == "unread" {
                    let read_url = graph::text(&p, "read_url");
                    unread.push((graph::number(&p, "last_ms"), graph::text(&p, "key"), Unread { title: graph::text(&p, "title"), url: read_url, why: graph::text(&p, "why") }));
                }
            } else if labels.iter().any(|l| l == FINDING) {
                let finding = Finding { statement: graph::text(&p, "statement"), notes: graph::texts(&p, "notes"), confidence: graph::text(&p, "confidence") };
                findings.push((graph::number(&p, "position"), finding));
            }
        }
        questions.sort_by_key(|(position, _, _)| *position);
        candidates.sort_by_key(|(position, _)| *position);
        unread.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        findings.sort_by_key(|(position, _)| *position);
        let total = questions.last().map(|(position, _, _)| position + 1).unwrap_or(0);
        let pending: Vec<(u32, Planned)> = questions.into_iter().filter(|(_, pending, _)| *pending).map(|(position, _, planned)| (position, planned)).collect();
        let base = pending.first().map(|(position, _)| *position).unwrap_or(total);
        let synthesis = graph::opt_text(&job, "summary").map(|summary| Synthesis {
            summary,
            findings: findings.into_iter().map(|(_, f)| f).collect(),
            open_questions: graph::texts(&job, "open_questions"),
        });
        let state = JobState {
            job: job_from(&job),
            params: params_from(&job),
            pending: pending.into_iter().map(|(_, planned)| planned).collect(),
            candidates: candidates.into_iter().map(|(_, c)| c).collect(),
            visited: graph::texts(&job, "visited"),
            unread: unread.into_iter().map(|(_, _, u)| u).collect(),
            queries_run: graph::texts(&job, "queries_run"),
            sources_searched: graph::texts(&job, "sources_searched"),
            synthesis,
            dropped_quotes: graph::number(&job, "dropped_quotes") as u32,
        };
        Ok(Kept { state, questions: Questions { base, total } })
    }

    /// Write every job's `state` and `phase` as temporal values. Earlier versions wrote them as
    /// plain properties, which the graph drops when it first declares them temporal: the values
    /// just read are written back as their first samples (repeating it adds nothing).
    /// One job per put: a job the graph refuses (its project is gone) costs only that job.
    async fn keep_history(&self, loaded: &HashMap<String, Kept>) {
        for kept in loaded.values() {
            let job = &kept.state.job;
            if job.project_id.is_empty() {
                continue;
            }
            let node = GraphNode { label: JOB.into(), key: job.job_id.clone(), props: Map::new(), temporal: job_temporal(&kept.state) };
            let link = edge(gref(JOB, &job.job_id), "FOR", gref("Project", &job.project_id));
            if let Err(error) = self.graph.put(vec![node], vec![link], PATIENT).await {
                tracing::warn!("research: job {}'s state and phase were not written as their history: {}", job.job_id, error.message);
            }
        }
    }

    /// Jobs kept as files by an earlier version: written to the graph (each one put, confirmed),
    /// then their files deleted. A file that cannot be read as a job is left for a look.
    async fn migrate(&self, loaded: &HashMap<String, Kept>) -> Vec<JobState> {
        let Ok(entries) = std::fs::read_dir(&self.legacy) else { return Vec::new() };
        let mut moved = Vec::new();
        for path in entries.flatten().map(|e| e.path()) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.starts_with('.') && name.ends_with(".tmp") {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if !name.ends_with(".json") {
                continue;
            }
            let Some(state) = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<JobState>(&text).ok()) else {
                tracing::warn!("research: {} is not a job this version reads; it stays where it is", path.display());
                continue;
            };
            // The graph's copy is newer when an earlier move wrote it but could not delete the file.
            if !loaded.contains_key(&state.job.job_id) {
                let questions = Questions { base: 0, total: state.pending.len() as u32 };
                let Batch { nodes, edges, supports, .. } = batch(None, &state, questions);
                if let Err(error) = self.graph.put(nodes, edges, PATIENT).await {
                    tracing::warn!("research: cannot move job {} into the graph ({}); its file stays until the next start", state.job.job_id, error.message);
                    continue;
                }
                write_supports(&self.graph, supports).await;
                moved.push(state);
            }
            if let Err(error) = std::fs::remove_file(&path) {
                tracing::warn!("research: cannot delete {} after moving it into the graph: {error}", path.display());
            }
        }
        let _ = std::fs::remove_dir(&self.legacy);
        moved
    }

    pub fn get(&self, job_id: &str) -> Option<JobState> {
        self.kept.lock().ok()?.get(job_id).map(|kept| kept.state.clone())
    }

    /// Every job, newest first.
    pub fn all(&self) -> Vec<JobState> {
        let mut jobs: Vec<JobState> = self.kept.lock().map(|kept| kept.values().map(|k| k.state.clone()).collect()).unwrap_or_default();
        jobs.sort_by(|a, b| b.job.started_ms.cmp(&a.job.started_ms));
        jobs
    }

    /// Keep a checkpoint in RAM and queue its writes: the job node and the children that changed.
    pub fn save(&self, state: &JobState) {
        let Ok(mut kept) = self.kept.lock() else { return };
        let previous = kept.get(&state.job.job_id);
        let mut questions = previous.map(|k| k.questions).unwrap_or_default();
        let pending_changed = previous.is_none_or(|k| k.state.pending != state.pending);
        if pending_changed && !state.pending.is_empty() {
            questions = Questions { base: questions.total, total: questions.total + state.pending.len() as u32 };
        } else if pending_changed {
            questions.base = questions.total;
        }
        let batch = batch(previous.map(|k| (&k.state, k.questions)), state, questions);
        kept.insert(state.job.job_id.clone(), Kept { state: state.clone(), questions });
        let _ = self.writes.send(Write::Batch(batch));
    }

    /// Wait until the queued writes are in the graph (at most `limit`).
    pub async fn flush(&self, limit: Duration) {
        if self.writer.lock().map(|w| w.is_some()).unwrap_or(true) {
            return;
        }
        let (done, wait) = oneshot::channel();
        if self.writes.send(Write::Flush(done)).is_ok() && tokio::time::timeout(limit, wait).await.is_err() {
            tracing::warn!("research: the job writes did not reach memory within {limit:?}; the successor reads what did");
        }
    }
}

/// The writer: each batch in order; while memory is away it waits and tries again.
async fn write_loop(graph: Arc<Graph>, mut receiver: mpsc::UnboundedReceiver<Write>) {
    while let Some(write) = receiver.recv().await {
        match write {
            Write::Flush(done) => {
                let _ = done.send(());
            }
            Write::Batch(batch) => loop {
                let outcome = async {
                    graph.put(batch.nodes.clone(), batch.edges.clone(), PATIENT).await?;
                    graph.retire(batch.retire.clone(), "replaced", PATIENT).await?;
                    Ok::<_, RpcError>(())
                }
                .await;
                match outcome {
                    Ok(()) => {
                        write_supports(&graph, batch.supports.clone()).await;
                        break;
                    }
                    Err(error) if graph::away(&error) => tracing::warn!("research: a job checkpoint waits for memory: {}", error.message),
                    Err(error) => {
                        tracing::warn!("research: memory refused a job checkpoint: {}", error.message);
                        break;
                    }
                }
            },
        }
    }
}

/// The findings' links to the notes they cite, one put per finding: a note that is not stored
/// refuses only its finding's links, which are logged and dropped (the finding keeps the ids in
/// `notes`). Links to notes a finding no longer cites stay: the contract cannot retire an edge yet.
async fn write_supports(graph: &Graph, supports: Vec<GraphEdge>) {
    let mut by_finding: BTreeMap<GraphRef, Vec<GraphEdge>> = BTreeMap::new();
    for support in supports {
        by_finding.entry(support.from.clone()).or_default().push(support);
    }
    for (finding, links) in by_finding {
        if let Err(error) = graph.put(Vec::new(), links, PATIENT).await {
            tracing::warn!("research: finding {}'s links to its notes were not written: {}", finding.key, error.message);
        }
    }
}

/// A job row (`j, j.state, j.phase`): the node's properties with its temporal `state` and `phase`.
fn job_row(row: &[Value]) -> Map<String, Value> {
    let mut props = graph::props_of(row.first().unwrap_or(&Value::Null));
    for (at, name) in [(1, "state"), (2, "phase")] {
        if let Some(value) = row.get(at).filter(|value| !value.is_null()) {
            props.insert(name.into(), value.clone());
        }
    }
    props
}

/// What a job's nodes should hold, by node.
fn desired(state: &JobState, questions: Questions) -> BTreeMap<GraphRef, Map<String, Value>> {
    let id = &state.job.job_id;
    let mut out = BTreeMap::new();
    for (i, planned) in state.pending.iter().enumerate() {
        let position = questions.base + i as u32;
        let props = json!({ "position": position, "question": planned.question, "queries": planned.queries, "sources": planned.sources, "pending": true });
        out.insert(gref(QUESTION, &format!("{id}#{position}")), into_map(props));
    }
    for (i, candidate) in state.candidates.iter().enumerate() {
        let mut props = candidate_props(candidate);
        props.insert("position".into(), json!(i));
        props.insert("state".into(), json!("candidate"));
        props.insert("why".into(), Value::Null);
        out.insert(gref(SOURCE, &graph::fit_key(&format!("{id}#{}", candidate.read_url))), props);
    }
    for unread in &state.unread {
        let at = gref(SOURCE, &graph::fit_key(&format!("{id}#{}", unread.url)));
        let props = out.entry(at).or_insert_with(|| {
            into_map(json!({ "url": unread.url, "read_url": unread.url, "title": unread.title, "position": Value::Null }))
        });
        props.insert("state".into(), json!("unread"));
        props.insert("why".into(), json!(unread.why));
    }
    if let Some(synthesis) = &state.synthesis {
        for (i, finding) in synthesis.findings.iter().enumerate() {
            let props = json!({ "position": i, "statement": finding.statement, "notes": finding.notes, "confidence": finding.confidence });
            out.insert(gref(FINDING, &format!("{id}#{i}")), into_map(props));
        }
    }
    out
}

/// The writes from one checkpoint to the next: the whole job node, its changed children,
/// questions searched since (`pending: false`) and children it no longer has (retired).
fn batch(previous: Option<(&JobState, Questions)>, state: &JobState, questions: Questions) -> Batch {
    let id = &state.job.job_id;
    let job_ref = gref(JOB, id);
    let mut nodes = vec![GraphNode { label: JOB.into(), key: id.clone(), props: job_props(state), temporal: job_temporal(state) }];
    let mut edges = vec![edge(job_ref.clone(), "FOR", gref("Project", &state.job.project_id))];
    if let Some(session) = state.job.session_id.as_deref().filter(|s| !s.is_empty()) {
        edges.push(edge(job_ref.clone(), "IN", gref("Session", session)));
    }
    let before = previous.map(|(s, q)| desired(s, q)).unwrap_or_default();
    let now = desired(state, questions);
    let mut supports = Vec::new();
    for (at, props) in &now {
        let old = before.get(at);
        if old == Some(props) {
            continue;
        }
        let mut props = props.clone();
        for name in old.into_iter().flat_map(|o| o.keys()) {
            props.entry(name.clone()).or_insert(Value::Null);
        }
        if at.label == FINDING {
            for note in props.get("notes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter(|note| !note.is_empty()) {
                supports.push(edge(at.clone(), "SUPPORTED_BY", gref("Note", note)));
            }
        }
        nodes.push(node(&at.label, &at.key, props));
        edges.push(edge(at.clone(), "OF", job_ref.clone()));
    }
    let mut retire = Vec::new();
    for at in before.keys().filter(|at| !now.contains_key(*at)) {
        if at.label == QUESTION {
            nodes.push(node(QUESTION, &at.key, into_map(json!({ "pending": false }))));
            edges.push(edge(at.clone(), "OF", job_ref.clone()));
        } else {
            retire.push(at.clone());
        }
    }
    Batch { nodes, edges, retire, supports }
}

fn into_map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn job_props(state: &JobState) -> Map<String, Value> {
    let (job, params) = (&state.job, &state.params);
    let synthesis = state.synthesis.as_ref();
    into_map(json!({
        "project_id": job.project_id,
        "session_id": graph::or_null(&job.session_id),
        "topic": job.topic,
        "round": job.round,
        "depth": job.depth,
        "questions": job.questions,
        "sources_read": job.sources_read,
        "sources_failed": job.sources_failed,
        "notes": job.notes,
        "report_path": graph::or_null(&job.report_path),
        "error": graph::or_null(&job.error),
        "resumed": job.resumed,
        "started_ms": job.started_ms,
        "updated_ms": job.updated_ms,
        "param_project_id": params.project_id,
        "param_topic": params.topic,
        "param_questions": params.questions,
        "param_depth": graph::or_null(&params.depth),
        "param_sources": params.sources,
        "param_since": graph::or_null(&params.since),
        "param_max_sources": graph::or_null(&params.max_sources),
        "param_root": graph::or_null(&params.root),
        "param_fresh": params.fresh,
        "param_session_id": graph::or_null(&params.session_id),
        "visited": state.visited,
        "queries_run": state.queries_run,
        "sources_searched": state.sources_searched,
        "dropped_quotes": state.dropped_quotes,
        "summary": synthesis.map(|s| json!(s.summary)).unwrap_or(Value::Null),
        "open_questions": synthesis.map(|s| json!(s.open_questions)).unwrap_or(Value::Null),
    }))
}

/// The job's temporal properties (see the module documentation).
fn job_temporal(state: &JobState) -> Map<String, Value> {
    into_map(json!({ "state": state.job.state, "phase": state.job.phase }))
}

fn job_from(p: &Map<String, Value>) -> Job {
    Job {
        job_id: graph::text(p, "key"),
        project_id: graph::text(p, "project_id"),
        topic: graph::text(p, "topic"),
        state: graph::text(p, "state"),
        phase: graph::text(p, "phase"),
        round: graph::number(p, "round") as u32,
        depth: graph::number(p, "depth") as u32,
        questions: graph::texts(p, "questions"),
        sources_read: graph::number(p, "sources_read") as u32,
        sources_failed: graph::number(p, "sources_failed") as u32,
        notes: graph::number(p, "notes") as u32,
        report_path: graph::opt_text(p, "report_path"),
        error: graph::opt_text(p, "error"),
        session_id: graph::opt_text(p, "session_id"),
        resumed: graph::number(p, "resumed") as u32,
        started_ms: graph::number(p, "started_ms"),
        updated_ms: graph::number(p, "updated_ms"),
    }
}

fn params_from(p: &Map<String, Value>) -> DeepParams {
    DeepParams {
        project_id: graph::text(p, "param_project_id"),
        topic: graph::text(p, "param_topic"),
        questions: graph::texts(p, "param_questions"),
        depth: graph::opt_number(p, "param_depth").map(|n| n as u32),
        sources: graph::texts(p, "param_sources"),
        since: graph::opt_text(p, "param_since"),
        max_sources: graph::opt_number(p, "param_max_sources").map(|n| n as u32),
        root: graph::opt_text(p, "param_root"),
        fresh: graph::flag(p, "param_fresh"),
        session_id: graph::opt_text(p, "param_session_id"),
    }
}

/// A candidate's own properties (its hit, what is read, which questions, how often found). The
/// hit's `extra` members become `extra_<name>`: scalars, and lists of one scalar type.
fn candidate_props(candidate: &Candidate) -> Map<String, Value> {
    let hit = &candidate.hit;
    let mut props = into_map(json!({
        "url": hit.url,
        "read_url": candidate.read_url,
        "title": hit.title,
        "source": hit.source,
        "date": graph::or_null(&hit.date),
        "authors": hit.authors,
        "snippet": hit.snippet,
        "score": graph::or_null(&hit.score),
        "questions": candidate.questions,
        "found_by": candidate.found_by,
    }));
    for (name, value) in &hit.extra {
        let snake = name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') && name.len() <= 50;
        if snake && scalar_or_list(value) {
            props.insert(format!("extra_{name}"), value.clone());
        }
    }
    props
}

fn scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

fn scalar_or_list(value: &Value) -> bool {
    match value {
        Value::Array(items) => {
            let kind = |v: &Value| match v {
                Value::String(_) => 1,
                Value::Number(_) => 2,
                Value::Bool(_) => 3,
                _ => 0,
            };
            items.first().is_none_or(|first| kind(first) != 0 && items.iter().all(|item| kind(item) == kind(first)))
        }
        other => scalar(other),
    }
}

fn candidate_from(p: &Map<String, Value>) -> Candidate {
    let extra: Map<String, Value> = p.iter().filter_map(|(name, value)| name.strip_prefix("extra_").filter(|_| !value.is_null()).map(|n| (n.to_owned(), value.clone()))).collect();
    Candidate {
        hit: SearchHit {
            source: graph::text(p, "source"),
            title: graph::text(p, "title"),
            url: graph::text(p, "url"),
            date: graph::opt_text(p, "date"),
            authors: graph::texts(p, "authors"),
            snippet: graph::text(p, "snippet"),
            score: p.get("score").and_then(Value::as_f64),
            extra,
        },
        read_url: graph::text(p, "read_url"),
        questions: p.get("questions").and_then(Value::as_array).map(|l| l.iter().filter_map(Value::as_u64).map(|n| n as usize).collect()).unwrap_or_default(),
        found_by: graph::number(p, "found_by") as u32,
    }
}
