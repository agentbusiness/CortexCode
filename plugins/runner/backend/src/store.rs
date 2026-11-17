//! The runner's records in the app's graph, through the memory plugin (`memory.graph.*`):
//!
//! ```text
//! (:RunnerRepo {key: repo id, url, main, mirror})-[:ON]->(:Machine)
//! (:RunnerJob {key: job id, attempt, repo, url, main, branch, base, root, mirror, project_id,
//!     session_id, title, goal, approach, offset, protected, standard_change, approve, trailers,
//!     started_ms, squashed, landed, stop_reason})-[:ON]->(:Machine)   -[:OF]->(:RunnerRepo)
//! (:RunnerTask {key: job#n, n, title, brief, known, rules, depends_on, touches, checks, request})
//!     -[:OF]->(:RunnerJob)
//! (:RunnerCheckpoint {key: job@task, task, commit, ms, files, lines})-[:OF]->(:RunnerJob)
//! ```
//!
//! A job that was removed is retired with its tasks and checkpoints. The worktrees and the mirror
//! are files (git objects are bytes, not records); the graph names them.

use cc_protocol::contracts::memory::{GraphEdge, GraphLayer, GraphNode, GraphPutParams, GraphRef, GraphRetireParams, methods};
use cc_protocol::contracts::runner::{Checkpoint, Task};
use cc_protocol::host::Host;
use cc_protocol::{RpcError, codes};
use serde_json::{Map, Value, json};

/// A repository the runner has a mirror of.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Repo {
    pub repo: String,
    pub url: String,
    pub main: String,
    pub mirror: String,
}

/// A job as it is kept.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Job {
    pub job_id: String,
    pub attempt: u32,
    pub repo: String,
    pub url: String,
    pub main: String,
    pub branch: String,
    /// The commit of the main branch the job's branch starts from (after a rebase: the new one).
    pub base: String,
    /// The job's worktree.
    pub root: String,
    pub mirror: String,
    pub project_id: String,
    /// The session of the job's newest run; empty until `runner.start`.
    pub session_id: String,
    pub title: String,
    pub goal: String,
    pub approach: String,
    /// The whole plan, numbered as the manager numbers it.
    pub tasks: Vec<Task>,
    /// How many of them came before the first task of the newest session (after a restart).
    pub offset: u32,
    pub protected: Vec<String>,
    pub standard_change: bool,
    pub approve: Vec<String>,
    pub trailers: Vec<String>,
    pub checkpoints: Vec<Checkpoint>,
    pub started_ms: u64,
    /// The branch's head after its commits were squashed for landing.
    pub squashed: String,
    /// The main branch's head this job landed as.
    pub landed: String,
    /// Why the runner itself stopped the run (`cancelled`); empty otherwise.
    pub stop_reason: String,
}

fn node_ref(label: &str, key: &str) -> GraphRef {
    GraphRef { label: label.to_owned(), key: key.to_owned() }
}

fn edge(from: GraphRef, rel: &str, to: GraphRef) -> GraphEdge {
    GraphEdge { from, rel: rel.to_owned(), to, props: Map::new(), temporal: Map::new() }
}

/// A list property; an empty list is written as no property (a list has one scalar type, and an
/// empty one has none).
fn list<T: serde::Serialize>(props: &mut Map<String, Value>, name: &str, items: &[T]) {
    props.insert(name.to_owned(), if items.is_empty() { Value::Null } else { json!(items) });
}

fn task_key(job: &str, n: u32) -> String {
    format!("{job}#{n}")
}

fn checkpoint_key(job: &str, task: u32) -> String {
    format!("{job}@{task}")
}

async fn machine(host: &Host) -> Result<String, RpcError> {
    let answer = host.call(methods::GRAPH_MACHINE, json!({})).await?;
    answer.get("key").and_then(Value::as_str).filter(|key| !key.is_empty()).map(str::to_owned).ok_or_else(|| RpcError::new(codes::CONTRACT_VIOLATION, "memory named no machine"))
}

async fn put(host: &Host, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> Result<(), RpcError> {
    let params = GraphPutParams { nodes, edges, layer: GraphLayer::Workspace, at_ms: None };
    host.call(methods::GRAPH_PUT, serde_json::to_value(params)?).await.map(drop)
}

async fn retire(host: &Host, refs: Vec<GraphRef>, reason: &str) -> Result<(), RpcError> {
    if refs.is_empty() {
        return Ok(());
    }
    let params = GraphRetireParams { refs, reason: Some(reason.to_owned()), at_ms: None };
    host.call(methods::GRAPH_RETIRE, serde_json::to_value(params)?).await.map(drop)
}

/// The properties of the nodes `cypher` returns first in each row.
async fn nodes(host: &Host, cypher: &str, params: Value) -> Result<Vec<Map<String, Value>>, RpcError> {
    let answer = host.call(methods::GRAPH_QUERY, json!({"cypher": cypher, "params": params, "limit": 5000})).await?;
    let rows = answer.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(rows.into_iter().filter_map(|row| row.as_array()?.first()?.get("properties")?.as_object().cloned()).collect())
}

fn text(props: &Map<String, Value>, name: &str) -> String {
    props.get(name).and_then(Value::as_str).unwrap_or("").to_owned()
}

fn number(props: &Map<String, Value>, name: &str) -> u64 {
    props.get(name).and_then(|value| value.as_u64().or_else(|| value.as_f64().map(|float| float.max(0.0) as u64))).unwrap_or(0)
}

fn texts(props: &Map<String, Value>, name: &str) -> Vec<String> {
    props.get(name).and_then(Value::as_array).map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

pub async fn put_repo(host: &Host, repo: &Repo) -> Result<(), RpcError> {
    let machine = machine(host).await?;
    let mut props = Map::new();
    props.insert("url".into(), json!(repo.url));
    props.insert("main".into(), json!(repo.main));
    props.insert("mirror".into(), json!(repo.mirror));
    let this = node_ref("RunnerRepo", &repo.repo);
    put(host, vec![GraphNode { label: "RunnerRepo".into(), key: repo.repo.clone(), props, temporal: Map::new() }], vec![edge(this, "ON", node_ref("Machine", &machine))]).await
}

pub async fn repos(host: &Host) -> Result<Vec<Repo>, RpcError> {
    let found = nodes(host, "MATCH (r:RunnerRepo) WHERE r.ended_ms IS NULL RETURN r", json!({})).await?;
    Ok(found.iter().map(|props| Repo { repo: text(props, "key"), url: text(props, "url"), main: text(props, "main"), mirror: text(props, "mirror") }).filter(|repo| !repo.repo.is_empty()).collect())
}

fn job_node(job: &Job) -> GraphNode {
    let mut props = Map::new();
    for (name, value) in [
        ("repo", &job.repo),
        ("url", &job.url),
        ("main", &job.main),
        ("branch", &job.branch),
        ("base", &job.base),
        ("root", &job.root),
        ("mirror", &job.mirror),
        ("project_id", &job.project_id),
        ("session_id", &job.session_id),
        ("title", &job.title),
        ("goal", &job.goal),
        ("approach", &job.approach),
        ("squashed", &job.squashed),
        ("landed", &job.landed),
        ("stop_reason", &job.stop_reason),
    ] {
        props.insert(name.into(), json!(value));
    }
    props.insert("attempt".into(), json!(job.attempt));
    props.insert("offset".into(), json!(job.offset));
    props.insert("standard_change".into(), json!(job.standard_change));
    props.insert("started_ms".into(), json!(job.started_ms));
    list(&mut props, "protected", &job.protected);
    list(&mut props, "approve", &job.approve);
    list(&mut props, "trailers", &job.trailers);
    GraphNode { label: "RunnerJob".into(), key: job.job_id.clone(), props, temporal: Map::new() }
}

fn task_node(job: &str, task: &Task) -> GraphNode {
    let mut props = Map::new();
    props.insert("n".into(), json!(task.n));
    props.insert("title".into(), json!(task.title));
    props.insert("brief".into(), json!(task.brief));
    props.insert("request".into(), json!(task.request));
    list(&mut props, "known", &task.known);
    list(&mut props, "rules", &task.rules);
    list(&mut props, "depends_on", &task.depends_on);
    list(&mut props, "touches", &task.touches);
    list(&mut props, "checks", &task.checks);
    GraphNode { label: "RunnerTask".into(), key: task_key(job, task.n), props, temporal: Map::new() }
}

fn checkpoint_node(job: &str, checkpoint: &Checkpoint) -> GraphNode {
    let mut props = Map::new();
    props.insert("task".into(), json!(checkpoint.task));
    props.insert("commit".into(), json!(checkpoint.commit));
    props.insert("ms".into(), json!(checkpoint.ms));
    props.insert("files".into(), json!(checkpoint.files));
    props.insert("lines".into(), json!(checkpoint.lines));
    GraphNode { label: "RunnerCheckpoint".into(), key: checkpoint_key(job, checkpoint.task), props, temporal: Map::new() }
}

/// Write the job whole: itself, its tasks and its checkpoints. `gone` are the tasks and
/// checkpoints it had before and has no more (a plan written again, a restart): retired.
pub async fn put_job(host: &Host, job: &Job, before: Option<&Job>) -> Result<(), RpcError> {
    let machine = machine(host).await?;
    let this = node_ref("RunnerJob", &job.job_id);
    let mut nodes = vec![job_node(job)];
    let mut edges = vec![edge(this.clone(), "ON", node_ref("Machine", &machine))];
    if !job.repo.is_empty() {
        // The repository's node is named too (its own properties stay as they are), so the edge
        // to it never points at nothing.
        let repo = node_ref("RunnerRepo", &job.repo);
        nodes.push(GraphNode { label: "RunnerRepo".into(), key: job.repo.clone(), props: Map::new(), temporal: Map::new() });
        edges.push(edge(repo.clone(), "ON", node_ref("Machine", &machine)));
        edges.push(edge(this.clone(), "OF", repo));
    }
    for task in &job.tasks {
        nodes.push(task_node(&job.job_id, task));
        edges.push(edge(node_ref("RunnerTask", &task_key(&job.job_id, task.n)), "OF", this.clone()));
    }
    for checkpoint in &job.checkpoints {
        nodes.push(checkpoint_node(&job.job_id, checkpoint));
        edges.push(edge(node_ref("RunnerCheckpoint", &checkpoint_key(&job.job_id, checkpoint.task)), "OF", this.clone()));
    }
    put(host, nodes, edges).await?;
    if let Some(before) = before {
        let mut gone: Vec<GraphRef> = before.tasks.iter().filter(|old| !job.tasks.iter().any(|task| task.n == old.n)).map(|old| node_ref("RunnerTask", &task_key(&job.job_id, old.n))).collect();
        gone.extend(before.checkpoints.iter().filter(|old| !job.checkpoints.iter().any(|checkpoint| checkpoint.task == old.task)).map(|old| node_ref("RunnerCheckpoint", &checkpoint_key(&job.job_id, old.task))));
        retire(host, gone, "replaced").await?;
    }
    Ok(())
}

/// The job is over: it, its tasks and its checkpoints are retired.
pub async fn retire_job(host: &Host, job: &Job) -> Result<(), RpcError> {
    let mut refs: Vec<GraphRef> = job.tasks.iter().map(|task| node_ref("RunnerTask", &task_key(&job.job_id, task.n))).collect();
    refs.extend(job.checkpoints.iter().map(|checkpoint| node_ref("RunnerCheckpoint", &checkpoint_key(&job.job_id, checkpoint.task))));
    refs.push(node_ref("RunnerJob", &job.job_id));
    retire(host, refs, "removed").await
}

/// Every job that was not removed, with its tasks and checkpoints.
pub async fn jobs(host: &Host) -> Result<Vec<Job>, RpcError> {
    let found = nodes(host, "MATCH (j:RunnerJob) WHERE j.ended_ms IS NULL RETURN j", json!({})).await?;
    let mut jobs = Vec::new();
    for props in found {
        let job_id = text(&props, "key");
        if job_id.is_empty() {
            continue;
        }
        let mut tasks: Vec<Task> = nodes(host, "MATCH (t:RunnerTask)-[:OF]->(:RunnerJob {key: $job}) WHERE t.ended_ms IS NULL RETURN t", json!({"job": job_id}))
            .await?
            .iter()
            .map(|task| Task {
                n: number(task, "n") as u32,
                title: text(task, "title"),
                brief: text(task, "brief"),
                known: texts(task, "known"),
                rules: texts(task, "rules"),
                depends_on: task.get("depends_on").and_then(Value::as_array).map(|items| items.iter().filter_map(|item| item.as_u64().map(|n| n as u32)).collect()).unwrap_or_default(),
                touches: texts(task, "touches"),
                checks: texts(task, "checks"),
                request: text(task, "request"),
            })
            .collect();
        tasks.sort_by_key(|task| task.n);
        let mut checkpoints: Vec<Checkpoint> = nodes(host, "MATCH (c:RunnerCheckpoint)-[:OF]->(:RunnerJob {key: $job}) WHERE c.ended_ms IS NULL RETURN c", json!({"job": job_id}))
            .await?
            .iter()
            .map(|checkpoint| Checkpoint { task: number(checkpoint, "task") as u32, commit: text(checkpoint, "commit"), ms: number(checkpoint, "ms"), files: number(checkpoint, "files") as u32, lines: number(checkpoint, "lines") as u32 })
            .collect();
        checkpoints.sort_by_key(|checkpoint| (checkpoint.ms, checkpoint.task));
        jobs.push(Job {
            job_id,
            attempt: number(&props, "attempt") as u32,
            repo: text(&props, "repo"),
            url: text(&props, "url"),
            main: text(&props, "main"),
            branch: text(&props, "branch"),
            base: text(&props, "base"),
            root: text(&props, "root"),
            mirror: text(&props, "mirror"),
            project_id: text(&props, "project_id"),
            session_id: text(&props, "session_id"),
            title: text(&props, "title"),
            goal: text(&props, "goal"),
            approach: text(&props, "approach"),
            tasks,
            offset: number(&props, "offset") as u32,
            protected: texts(&props, "protected"),
            standard_change: props.get("standard_change").and_then(Value::as_bool).unwrap_or(false),
            approve: texts(&props, "approve"),
            trailers: texts(&props, "trailers"),
            checkpoints,
            started_ms: number(&props, "started_ms"),
            squashed: text(&props, "squashed"),
            landed: text(&props, "landed"),
            stop_reason: text(&props, "stop_reason"),
        });
    }
    Ok(jobs)
}
