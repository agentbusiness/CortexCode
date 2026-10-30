//! The `cc-agents` plugin (`agents@1`): delegation into a task tree with contracts, named agents in
//! copy-on-write workspaces, the scheduler, host-run verification, the board and the merge steward.
//!
//! Agents, tasks and board messages live in the graph (memory) and the agents' journals (frame);
//! this plugin holds only the scheduler's queue, which it rebuilds on activation from the agent
//! records that say `working`.

use crate::contract;
use crate::host::{self, Host};
use crate::journal::Journal;
use crate::llm::{self, CallSpec, StreamTo};
use crate::graph::{self, render_impact};
use crate::plan;
use crate::data::{Data, Dataset, Loaded};
use crate::schemas::{self, Plan, PlannedTask, Review, parse_json};
use cc_protocol::contracts::agents::{
    AgentRefParams, AgentSteerParams, CancelParams, HandoffParams, CancelResult, DelegateParams, DelegateResult, ListParams, ListResult, MergeOutcome,
    MergeParams, ProposeParams, ProposeResult, SchedulerState, SpawnParams, Supervise, VerifyParams, VerifyReport, events, methods, topics,
};
use cc_protocol::contracts::chatflow::{AgentRunResult, verbs as chatflow_verbs};
use cc_protocol::contracts::frame::{JournalEntry, Project, ProjectListResult, SessionCreateParams, SessionKind, flows, kinds, methods as frame_methods};
use cc_protocol::contracts::lang::{self, Description, verifiers::{self, Candidate}};
use cc_protocol::contracts::lifecycle::{Hello, ProbeCheck, ProbeReport};
use cc_protocol::contracts::memory::{
    AgentListResult, AgentRecord, BoardMessage, Contract, ContractCheck, ImpactResult, SearchResult, SymbolMatches, Task, TaskListResult,
    methods as memory_methods, topics as memory_topics,
};
use cc_protocol::contracts::mesh::methods as mesh_methods;
use cc_protocol::contracts::model::{ChatMessage, ModelInfo, Priority, methods as model_methods};
use cc_protocol::contracts::now_ms;
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError, codes, parse_params, to_value};
use crate::control::{Controller, Settings as ControlSettings, Window};
use crate::scheduler::{AgentKey, Scheduler};
use cc_protocol::contracts::chatflow::topics as chatflow_topics;
use std::collections::VecDeque;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Approval modes an agent's runs can use: `auto` (writes confined to the workspace by the tools'
/// write sandbox, nothing asked) or `ask` (actions outside it wait for the user's approval).
const MODES: &[&str] = &["auto", "ask"];
/// Who accepts revisions of a frozen contract on the planner's behalf (never a worker's name).
const FOREMAN: &str = "foreman";

/// Names for agents (naming only; never interpreted).
const NAMES: &[&str] = &[
    "ada", "grace", "alan", "edsger", "barbara", "ken", "dennis", "margaret", "donald", "frances", "radia", "linus",
    "guido", "bjarne", "niklaus", "john", "leslie", "tony", "fran", "anita",
];

pub fn hello() -> Hello {
    let mut hello = Hello::new("cc-agents", env!("CARGO_PKG_VERSION"), "agents", "agents@1");
    hello.provides = [
        methods::DELEGATE,
        methods::SPAWN,
        methods::HANDOFF,
        methods::LIST,
        methods::STEER,
        methods::STOP,
        methods::RESTART,
        methods::CANCEL,
        methods::VERIFY,
        methods::MERGE,
        methods::SCHEDULER,
        methods::PROPOSE,
    ]
    .iter()
    .map(|method| (*method).to_owned())
    .collect();
    // What a session's shell may call through `cx`: the session controls the agents it started.
    hello.commands = [methods::HANDOFF, methods::LIST, methods::STEER, methods::STOP, methods::RESTART, methods::CANCEL, methods::SCHEDULER]
        .iter()
        .map(|method| (*method).to_owned())
        .collect();
    // Live model-call metrics for the concurrency controller; board posts (an agent's need for its
    // lead wakes the session that delegated the work).
    hello.subscribes = vec![chatflow_topics::USAGE.to_owned(), memory_topics::BOARD_POSTED.to_owned()];
    hello
}

/// Event topic of limit changes: `{ts_ms, from, to, reason, metrics, model}`.
pub const SCHEDULER_TOPIC: &str = "agents.scheduler";

/// One finished model call, as `chatflow.usage` reports it.
#[derive(Debug, Clone, serde::Deserialize)]
struct UsageSample {
    #[serde(default)]
    ts_ms: u64,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prefill_ms: f64,
    #[serde(default)]
    decode_ms: f64,
    #[serde(default)]
    queue_ms: f64,
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Default)]
struct ModelLoad {
    slots: u32,
    /// Conversations of the working context the shared pool holds.
    room: u32,
    busy: u32,
    queued: u32,
    hibernated: u32,
    /// The model's restores of spilled conversations so far, sampled each tick: (ms, count, total
    /// restore ms).
    restores: VecDeque<(u64, u32, f64)>,
}

impl ModelLoad {
    fn to_json(&self) -> Value {
        json!({"slots": self.slots, "room": self.room, "busy": self.busy, "queued": self.queued, "hibernated": self.hibernated})
    }

    /// Restores in the last `window_ms` and the share of it they took.
    fn restores_in(&self, window_ms: u64) -> (u32, f64) {
        let (Some(first), Some(last)) = (self.restores.front(), self.restores.back()) else { return (0, 0.0) };
        // A reload of the model restarts its counters.
        if last.1 < first.1 {
            return (0, 0.0);
        }
        (last.1 - first.1, (last.2 - first.2).max(0.0) / window_ms.max(1) as f64)
    }
}

/// Conversations of `working` tokens the model's shared pool holds. Each resident conversation's
/// fixed state (recurrent state, checkpoints) also takes pool room: `ctx_total / slots`, since
/// `slots` counts conversations that hold no context yet.
fn pool_room(info: &ModelInfo, working: u64) -> u32 {
    if info.slots == 0 || info.ctx_total == 0 {
        return 0;
    }
    let pool = u64::from(info.ctx_total);
    let state = pool / u64::from(info.slots);
    let working = if info.ctx_native == 0 { working } else { working.min(u64::from(info.ctx_native)) };
    u32::try_from(pool / (working + state).max(1)).unwrap_or(u32::MAX).max(1)
}

fn settings_of(config: &crate::data::SchedulerConfig) -> ControlSettings {
    ControlSettings {
        min: config.min,
        max: config.max,
        settle_ms: config.settle_ms,
        foreground_queue_ms: config.foreground_queue_ms,
        foreground_prefill_slowdown: config.foreground_prefill_slowdown,
        foreground_tpot_ms: config.foreground_tpot_ms,
        gain: config.gain,
        probe_after_ms: config.probe_after_ms,
        pool_restore_share: config.pool_restore_share,
    }
}

/// Fewest uncached prompt tokens a foreground call must prefill for its prefill speed to count.
const MIN_MEASURED_PREFILL: u64 = 256;

/// Aggregate a window of samples (mechanical arithmetic).
fn window_of(usage: &VecDeque<UsageSample>, window_ms: u64, running: u32, queued: u32) -> Window {
    let now = now_ms();
    let recent: Vec<&UsageSample> = usage.iter().filter(|sample| now.saturating_sub(sample.ts_ms) <= window_ms).collect();
    let agents: Vec<&&UsageSample> = recent.iter().filter(|sample| sample.priority == "agent").collect();
    let foreground: Vec<&&UsageSample> = recent.iter().filter(|sample| sample.priority == "foreground").collect();
    let tokens: u64 = agents.iter().map(|sample| sample.completion_tokens).sum();
    // Prefill speed only from calls that prefilled enough tokens for the rate to mean something.
    let prefill: Vec<f64> = foreground
        .iter()
        .filter_map(|sample| {
            let uncached = sample.prompt_tokens.saturating_sub(sample.cached_tokens);
            (uncached >= MIN_MEASURED_PREFILL).then(|| sample.prefill_ms / uncached as f64)
        })
        .collect();
    let tpot: Vec<f64> = foreground.iter().filter(|sample| sample.completion_tokens > 0).map(|sample| sample.decode_ms / sample.completion_tokens as f64).collect();
    Window {
        agent_tps: tokens as f64 / (window_ms.max(1) as f64 / 1000.0),
        agent_calls: agents.len() as u32,
        foreground_calls: foreground.len() as u32,
        foreground_queue_ms: foreground.iter().map(|sample| sample.queue_ms).fold(0.0, f64::max),
        foreground_prefill_ms_per_token: if prefill.is_empty() { 0.0 } else { prefill.iter().sum::<f64>() / prefill.len() as f64 },
        foreground_tpot_ms: if tpot.is_empty() { 0.0 } else { tpot.iter().sum::<f64>() / tpot.len() as f64 },
        running,
        queued,
        restores: 0,
        restore_share: 0.0,
    }
}

fn window_json(window: &Window) -> Value {
    json!({
        "agent_tokens_per_s": window.agent_tps,
        "agent_calls": window.agent_calls,
        "foreground_calls": window.foreground_calls,
        "foreground_queue_ms": window.foreground_queue_ms,
        "foreground_prefill_ms_per_token": window.foreground_prefill_ms_per_token,
        "foreground_tpot_ms": window.foreground_tpot_ms,
        "running": window.running,
        "queued": window.queued,
        "restores": window.restores,
        "restore_share": window.restore_share,
    })
}

/// The controller never resumes after Stop; a later explicit operation owns a separate scope.
struct AgentCancellations {
    controller: CancellationToken,
    manual: Vec<CancellationToken>,
}

struct Inner {
    /// Prompts, texts and tunables (reloaded on change).
    data: Arc<Data>,
    scheduler: Mutex<Scheduler>,
    /// The concurrency control loop (created once the model's slots are known).
    controller: Mutex<Option<Controller>>,
    /// Live model-call metrics (`chatflow.usage` events), newest last.
    usage: Mutex<VecDeque<UsageSample>>,
    /// The model's slots and the last `model.slots` reading.
    model: Mutex<ModelLoad>,
    /// Every limit change with the metrics behind it, newest last.
    adjustments: Mutex<VecDeque<Value>>,
    /// Projects this process has opened in the memory plugin.
    opened: Mutex<std::collections::HashSet<String>>,
    /// Effort per task chosen by the planner (used when an agent's run starts).
    efforts: Mutex<HashMap<String, String>>,
    /// Stop scopes, shared by an agent's controller and explicit merge/verify calls.
    cancellations: Mutex<HashMap<AgentKey, AgentCancellations>>,
    /// One merge into a project at a time.
    merge: tokio::sync::Mutex<()>,
    /// Lead sessions this plugin checks on itself, because the scheduler could not take their
    /// check-in routine (`schedule.add` failed: no scheduler running).
    self_checked: Mutex<std::collections::HashSet<(String, String)>>,
    tasks: tokio_util::task::TaskTracker,
    park: CancellationToken,
}

#[derive(Clone)]
pub struct Agents {
    inner: Arc<Inner>,
}

/// Event topic of data set loads: `{ok, version, sources}` or `{ok: false, error, version}`.
pub const DATA_TOPIC: &str = "agents.data";

impl Agents {
    /// The agents plugin, starting from the shipped data set.
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            inner: Arc::new(Inner {
                data: Arc::new(Data::new()?),
                scheduler: Mutex::new(Scheduler::default()),
                controller: Mutex::new(None),
                usage: Mutex::new(VecDeque::new()),
                model: Mutex::new(ModelLoad::default()),
                adjustments: Mutex::new(VecDeque::new()),
                opened: Mutex::new(std::collections::HashSet::new()),
                efforts: Mutex::new(HashMap::new()),
                cancellations: Mutex::new(HashMap::new()),
                merge: tokio::sync::Mutex::new(()),
                self_checked: Mutex::new(std::collections::HashSet::new()),
                tasks: tokio_util::task::TaskTracker::new(),
                park: CancellationToken::new(),
            }),
        })
    }

    /// Data layers: `data_dir` from the plugin config, then the live data in the plugin's folder of
    /// the live folder (taken once it exists).
    fn data_layers(ctx: &Ctx) -> Vec<(String, std::path::PathBuf)> {
        let mut layers = Vec::new();
        if let Some(dir) = ctx.config().get("data_dir").and_then(Value::as_str) {
            let dir = match dir.strip_prefix("~/") {
                Some(rest) => std::env::var("HOME").map(|home| std::path::PathBuf::from(home).join(rest)).unwrap_or_else(|_| dir.into()),
                None => dir.into(),
            };
            layers.push(("shipped".to_owned(), dir));
        }
        if let Some(live) = &ctx.env.live_dir {
            layers.push((crate::data::LIVE.to_owned(), live.join("backend").join("data")));
        }
        layers
    }

    fn publish_data(&self, ctx: &Ctx, loaded: &Loaded) {
        if let Some(event) = self.inner.data.event(loaded) {
            if let Loaded::Rejected(error) = loaded {
                tracing::warn!(%error, "data set rejected; the previous one stays active");
            }
            ctx.publish(DATA_TOPIC, event);
        }
    }

    fn start_data(&self, ctx: &Ctx) {
        let loaded = self.inner.data.set_layers(Self::data_layers(ctx));
        self.publish_data(ctx, &loaded);
        let this = self.clone();
        let ctx = ctx.clone();
        let park = self.inner.park.clone();
        self.inner.tasks.spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(crate::data::WATCH_EVERY) => {}
                    _ = park.cancelled() => return,
                }
                let loaded = this.inner.data.reload(false);
                this.publish_data(&ctx, &loaded);
            }
        });
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

async fn agents_of(host: &Host, project_id: &str) -> Result<Vec<AgentRecord>, RpcError> {
    Ok(host.get::<AgentListResult>(memory_methods::AGENT_LIST, json!({"project_id": project_id})).await?.agents)
}

async fn agent(host: &Host, project_id: &str, name: &str) -> Result<AgentRecord, RpcError> {
    agents_of(host, project_id)
        .await?
        .into_iter()
        .find(|agent| agent.name == name)
        .ok_or_else(|| RpcError::not_found(format!("no agent named {name} in this project")))
}

async fn task(host: &Host, id: &str) -> Result<Task, RpcError> {
    host.get(memory_methods::TASK_GET, json!({"id": id})).await
}

async fn upsert(ctx: &Ctx, host: &Host, mut record: AgentRecord) -> Result<AgentRecord, RpcError> {
    record.updated_ms = now_ms();
    host.call(memory_methods::AGENT_UPSERT, serde_json::to_value(&record)?).await?;
    ctx.publish(
        topics::AGENT_CHANGED,
        json!({"agent": record.name, "project_id": record.project_id, "status": record.status, "task_id": record.task_id, "session_id": record.session_id}),
    );
    Ok(record)
}

async fn board(host: &Host, project_id: &str, kind: &str, author: &str, text: &str, refs: &[String]) {
    let posted = host
        .call(memory_methods::BOARD_POST, json!({"project_id": project_id, "kind": kind, "author": author, "text": text, "refs": refs}))
        .await;
    if let Err(error) = posted {
        tracing::warn!(%error, "board post failed");
    }
}

async fn update_task(host: &Host, fields: Value) -> Result<(), RpcError> {
    host.call(memory_methods::TASK_UPDATE, fields).await.map(|_| ())
}

/// The session that delegated an agent's work (its own session's parent).
/// The role of an agent doing work handed to it from another session ([`methods::HANDOFF`]).
const HANDOFF_ROLE: &str = "handoff";

/// A handed-off piece of work ended: its result goes back to the asking session as a note the
/// asker's model reads (`host_message`), with the work's session to look at.
async fn handoff_ended(host: &Host, record: &AgentRecord, task: &Task, outcome: &str, summary: &str) {
    if record.role != HANDOFF_ROLE {
        return;
    }
    let Some(asker) = lead_session(host, record).await else { return };
    let flow = match &record.session_id {
        Some(session_id) => host.session_get(session_id).await.map(|session| session.flow).unwrap_or_default(),
        None => String::new(),
    };
    let project = host.project_get(&record.project_id).await.map(|project| project.name).unwrap_or_default();
    let summary = summary.trim();
    let text = format!("The {flow} work handed to {project} ({}) {outcome}.{}", task.title, if summary.is_empty() { String::new() } else { format!("\n{summary}") });
    let _ = host
        .journal_append(&asker, kinds::NOTE, json!({"text": text, "purpose": "handoff_result", "host_message": true, "agent": record.name, "task_id": task.id, "session_id": record.session_id, "outcome": outcome}))
        .await;
}

/// A chat flow method for a session: every chat flow call goes to the session's own flow
/// (`chatflow-coding.steer`).
async fn flow_method(host: &Host, session_id: &str, verb: &str) -> String {
    let flow = host.session_get(session_id).await.map(|session| session.flow).unwrap_or_else(|_| flows::default_flow());
    flows::method(&flow, verb)
}

async fn lead_session(host: &Host, record: &AgentRecord) -> Option<String> {
    let session_id = record.session_id.as_deref()?;
    host.session_get(session_id).await.ok().and_then(|session| session.parent)
}

/// A progress line in the delegating session's journal: shown on its card, not a model message.
async fn progress(host: &Host, lead: Option<&str>, text: &str, record: &AgentRecord) {
    if let Some(lead) = lead {
        let _ = host
            .journal_append(lead, kinds::NOTE, json!({"text": text, "purpose": "agent_progress", "host_message": false, "agent": record.name, "task_id": record.task_id}))
            .await;
    }
}

/// A supervision event about `agent`'s work on `task` (the session is set when it is sent).
fn event_of(project_id: &str, event: &str, agent: &str, task: Option<&Task>, text: &str) -> Supervise {
    Supervise {
        id: format!("sup-{}", uuid::Uuid::new_v4().simple()),
        session_id: String::new(),
        project_id: project_id.to_owned(),
        event: event.to_owned(),
        agent: agent.to_owned(),
        task_id: task.map(|task| task.id.clone()).unwrap_or_default(),
        title: task.map(|task| task.title.clone()).unwrap_or_default(),
        text: text.to_owned(),
        failed_tests: None,
    }
}

/// Tell the delegating session that its work needs a decision ([`topics::SUPERVISE`]).
fn supervise(ctx: &Ctx, lead: Option<&str>, mut event: Supervise) {
    let Some(lead) = lead else { return };
    event.session_id = lead.to_owned();
    ctx.publish(topics::SUPERVISE, serde_json::to_value(&event).unwrap_or_default());
}

/// A task that will not run again.
fn finished(status: &str) -> bool {
    matches!(status, "done" | "cancelled")
}

impl Agents {
    // ---- delegation -------------------------------------------------------------------------

    /// Make sure the memory plugin has the project open (once per project per process; the
    /// plugin itself treats repeated opens as no-ops).
    async fn open_project(&self, host: &Host, project: &Project) {
        let first = lock(&self.inner.opened).insert(project.id.clone());
        if !first {
            return;
        }
        let opened = tokio::time::timeout(
            Duration::from_secs(600),
            host.call(memory_methods::PROJECT_OPEN, json!({"root": project.root, "project_id": project.id})),
        )
        .await;
        match opened {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                lock(&self.inner.opened).remove(&project.id);
                tracing::warn!(%error, "memory.project.open failed");
            }
            Err(_) => tracing::warn!("memory.project.open did not finish in 10 minutes; continuing"),
        }
    }

    async fn delegate(&self, ctx: Ctx, params: DelegateParams) -> Result<DelegateResult, RpcError> {
        let host = Host::new(ctx.clone());
        let project = host.project_get(&params.project_id).await?;
        self.open_project(&host, &project).await;
        let mut journal = Journal::load(host.clone(), &params.from_session).await?;
        let data = self.inner.data.get();
        let graph = graph_context(&host, &project, &params.goal, &data).await;
        let run_id = format!("delegate-{}", uuid::Uuid::new_v4().simple());
        let no_context = data.text("no_context", &[]);
        let request = data.text(
            "request",
            &[
                ("goal", params.goal.trim()),
                ("context", if params.context.trim().is_empty() { &no_context } else { params.context.trim() }),
                ("graph", &graph),
            ],
        );
        // `checks` is a pick of the project's own check commands (none: no checks, no contract).
        let candidates = verifier_candidates(&host, &project.root).await;
        let shape = data.plan_shape().filled(schemas::VERIFIERS_PLACEHOLDER, &verifiers::commands(&candidates));
        let offered = candidates_text(&data, &candidates);
        let mut live = String::new();
        let mut planned: Option<Vec<PlannedTask>> = None;
        let mut overlap_retry_done = false;
        let mut impact: Vec<BTreeSet<String>> = Vec::new();
        for _attempt in 0..data.plan_attempts {
            // The check commands sit right before the plan, after any note on the previous attempt.
            let note = [live.trim(), offered.as_str()].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n");
            let stage = if note.is_empty() {
                data.text("stage", &[("stage", "PLAN")])
            } else {
                data.text("stage_live", &[("stage", "PLAN"), ("live", &note)])
            };
            let messages = vec![
                ChatMessage::new("system", data.planner_system()),
                ChatMessage::new("user", request.clone()),
                ChatMessage::new("user", format!("{} {stage}", data.text("host_prefix", &[]))),
            ];
            let call = CallSpec {
                stage: "PLAN".into(),
                messages,
                tools: Vec::new(),
                json_schema: None,
                grammar: Some(schemas::grammar(&shape)),
                tool_choice: None,
                effort: data.plan_effort().to_owned(),
                sampling: data.plan_sampling(data.plan_effort() != "none"),
                priority: Priority::Foreground,
                cache_key: format!("{}:plan", params.from_session),
                stream: StreamTo { run_id: run_id.clone(), step: 1, content: false },
            };
            let called = llm::chat(&mut journal, call, &CancellationToken::new()).await?;
            let tasks = match parse_json::<Plan>(&called.result.content) {
                Ok(plan) => plan.tasks,
                Err(error) => {
                    live = data.text("plan_unreadable", &[("error", &error)]);
                    continue;
                }
            };
            let mut problems = plan::validate(&tasks);
            for task in &tasks {
                for check in &task.checks {
                    if let Some(why) = contract::refusal(&contract::rooted(check, &[project.root.as_str()]), &project.root) {
                        problems.push(format!("task \"{}\": check `{}` cannot be run: {why}", task.title.trim(), check.trim()));
                    }
                }
            }
            if !problems.is_empty() {
                live = data.text("plan_unusable", &[("problems", &problems.join("; "))]);
                continue;
            }
            impact = Vec::new();
            for task in &tasks {
                impact.push(impact_set(&host, &project, &task.touches).await);
            }
            let found = plan::overlaps(&tasks, &impact);
            if !found.is_empty() && !overlap_retry_done {
                overlap_retry_done = true;
                live = plan::describe_overlaps(&data, &tasks, &found);
                planned = Some(tasks);
                continue;
            }
            planned = Some(tasks);
            break;
        }
        let Some(mut tasks) = planned else {
            return Err(RpcError::new(codes::INTERNAL, "the model did not produce a usable plan"));
        };
        if impact.len() != tasks.len() {
            impact = Vec::new();
            for task in &tasks {
                impact.push(impact_set(&host, &project, &task.touches).await);
            }
        }
        let serialized = plan::serialize(&mut tasks, &impact);
        let order = plan::order(&tasks).ok_or_else(|| RpcError::internal("the plan's dependencies form a cycle"))?;

        let root: Task = host
            .get(
                memory_methods::TASK_CREATE,
                json!({
                    "project_id": project.id,
                    "title": params.goal.lines().next().unwrap_or("Delegated goal").trim(),
                    "brief": request,
                    "plan": tasks.iter().map(|task| format!("- {}", task.title.trim())).collect::<Vec<_>>().join("\n"),
                    "contract": Contract::default(),
                }),
            )
            .await?;
        journal.append(kinds::TASK_LINK, json!({"task_id": root.id, "title": root.title, "run_id": run_id})).await?;
        let mut ids: HashMap<String, String> = HashMap::new();
        let mut created: Vec<Task> = Vec::new();
        for index in order {
            let planned = &tasks[index];
            let depends: Vec<String> = planned.depends_on.iter().filter_map(|title| ids.get(title.trim()).cloned()).collect();
            let task: Task = host
                .get(
                    memory_methods::TASK_CREATE,
                    json!({
                        "project_id": project.id,
                        "parent_id": root.id,
                        "title": planned.title.trim(),
                        "brief": planned.brief,
                        "plan": planned.plan,
                        "contract": contract::contract_from(&planned.checks.iter().map(|command| contract::rooted(command, &[project.root.as_str()])).collect::<Vec<_>>()),
                        "depends_on": depends,
                        "touches": planned.touches,
                    }),
                )
                .await?;
            lock(&self.inner.efforts).insert(task.id.clone(), schemas::effort_or_default(&planned.effort, &data.effort.default));
            ids.insert(planned.title.trim().to_owned(), task.id.clone());
            journal.append(kinds::TASK_LINK, json!({"task_id": task.id, "title": task.title, "run_id": run_id, "parent_id": root.id})).await?;
            created.push(task);
        }
        let summary = plan::impact_summary(&tasks, &impact);
        let mut note = format!("Planned {} task(s) for: {}", created.len(), params.goal.trim());
        for (title, items) in &summary {
            note.push_str(&format!("\n- {title}: affects {}", if items.is_empty() { "(nothing found in the graph)".to_owned() } else { items.join(", ") }));
        }
        for line in &serialized {
            note.push_str(&format!("\nSerialized: {line}"));
        }
        journal
            .append(kinds::NOTE, json!({"text": note, "purpose": "plan", "run_id": run_id, "host_message": false, "data_version": data.version()}))
            .await?;

        let mut agents = Vec::new();
        let allowed = params.max_agents.map(|max| max as usize).unwrap_or(usize::MAX);
        for task in created.iter().filter(|task| task.depends_on.is_empty()).take(allowed) {
            let record = self
                .spawn(ctx.clone(), SpawnParams {
                    project_id: project.id.clone(),
                    task_id: task.id.clone(),
                    name: None,
                    role: None,
                    from_session: Some(params.from_session.clone()),
                    mode: params.mode.clone(),
                    flow: None,
                })
                .await?;
            agents.push(record);
        }
        let mut fresh = Vec::new();
        for task in &created {
            fresh.push(self::task(&host, &task.id).await.unwrap_or_else(|_| task.clone()));
        }
        Ok(DelegateResult { root_task: root, tasks: fresh, agents })
    }

    // ---- spawning and running ---------------------------------------------------------------

    /// Hand a piece of work to a flow in a project: a task there with the asker's brief, and an
    /// agent of that flow on it whose session is the asking session's child. The asker keeps
    /// going; the result comes back to it as a note ([`Self::steward`], [`handoff_ended`]).
    async fn handoff(&self, ctx: Ctx, params: HandoffParams) -> Result<AgentRecord, RpcError> {
        let host = Host::new(ctx.clone());
        let asker = host.session_get(&params.from_session).await?;
        let project = match params.project.as_deref().map(str::trim).filter(|project| !project.is_empty()) {
            None => host.project_get(&asker.project_id).await?,
            Some(wanted) => {
                let listed: ProjectListResult = host.get(frame_methods::PROJECT_LIST, json!({})).await?;
                let names: Vec<String> = listed.projects.iter().map(|project| project.name.clone()).collect();
                listed
                    .projects
                    .into_iter()
                    .find(|project| project.id == wanted || project.name.eq_ignore_ascii_case(wanted))
                    .ok_or_else(|| RpcError::not_found(format!("no project named {wanted}; the projects are: {}", names.join(", "))))?
            }
        };
        let flow = params.flow.as_deref().map(str::trim).filter(|flow| !flow.is_empty()).map_or_else(|| asker.flow.clone(), str::to_owned);
        if !project.flows.contains(&flow) {
            return Err(RpcError::invalid_params(format!("the project {} does not take the {flow} flow; it takes: {}", project.name, project.flows.join(", "))));
        }
        let brief = params.brief.trim();
        if brief.is_empty() {
            return Err(RpcError::invalid_params("say what to do: the brief is empty"));
        }
        let title = params.title.as_deref().map(str::trim).filter(|title| !title.is_empty()).map_or_else(|| brief.lines().next().unwrap_or(brief).chars().take(80).collect(), str::to_owned);
        // No checks are named for a hand-off: none means no contract (the work is judged by its
        // result, which goes back to the asker).
        let task: Task = host.get(memory_methods::TASK_CREATE, json!({"project_id": project.id, "title": title, "brief": brief, "contract": Contract::default()})).await?;
        self.spawn(ctx, SpawnParams { project_id: project.id.clone(), task_id: task.id, name: None, role: Some(HANDOFF_ROLE.to_owned()), from_session: Some(asker.id), mode: params.mode, flow: Some(flow) }).await
    }

    async fn spawn(&self, ctx: Ctx, params: SpawnParams) -> Result<AgentRecord, RpcError> {
        if !MODES.contains(&params.mode.as_str()) {
            return Err(RpcError::invalid_params(format!("unknown mode `{}` (expected one of {})", params.mode, MODES.join(", "))));
        }
        let host = Host::new(ctx.clone());
        let project = host.project_get(&params.project_id).await?;
        self.open_project(&host, &project).await;
        let task = task(&host, &params.task_id).await?;
        let existing = agents_of(&host, &project.id).await.unwrap_or_default();
        let taken: BTreeSet<&str> = existing.iter().map(|agent| agent.name.as_str()).collect();
        let name = match params.name.clone().filter(|name| !name.trim().is_empty()) {
            Some(name) if taken.contains(name.as_str()) => return Err(RpcError::invalid_params(format!("an agent named {name} already exists"))),
            Some(name) => name,
            None => pick_name(&taken),
        };
        // Every session edits the project's own folder (one tree, many hands): the tools plugin
        // reserves a file under edit for its session's turn and runs checks on a throw-away copy
        // while others are active, so no clone, no snapshot and no merge are needed.
        let workspace_path = project.root.clone();
        let session = host
            .session_create(&SessionCreateParams {
                project_id: project.id.clone(),
                kind: SessionKind::Agent,
                title: task.title.clone(),
                agent: Some(name.clone()),
                parent: params.from_session.clone(),
                workspace: Some(workspace_path.clone()),
                task_id: Some(task.id.clone()),
                flow: params.flow.clone(),
            })
            .await?;
        let record = upsert(&ctx, &host, AgentRecord {
            name: name.clone(),
            project_id: project.id.clone(),
            role: params.role.clone().unwrap_or_else(|| "worker".into()),
            charter: String::new(),
            status: "working".into(),
            session_id: Some(session.id.clone()),
            task_id: Some(task.id.clone()),
            workspace: Some(workspace_path.clone()),
            tasks_done: 0,
            tasks_failed: 0,
            updated_ms: now_ms(),
            mode: params.mode.clone(),
        })
        .await?;
        // The claim freezes the contract: from here on only a revision accepted by someone other
        // than this worker changes it (`agents.contract.propose`).
        update_task(&host, json!({"id": task.id, "status": "in_progress", "assignee": name, "session_id": session.id, "freeze": true})).await?;
        if let Some(from) = &params.from_session {
            host.journal_append(from, kinds::AGENT_SPAWNED, json!({"agent": name, "task_id": task.id, "session_id": session.id, "workspace": workspace_path}))
                .await?;
            self.check_in(&host, &project.id, from, &task.id, &params.mode).await;
        }
        let data = self.inner.data.get();
        let touches = if task.touches.is_empty() { String::new() } else { data.text("board_touches", &[("touches", &task.touches.join(", "))]) };
        board(&host, &project.id, "claim", &name, &data.text("board_claim", &[("title", &task.title), ("touches", &touches)]), std::slice::from_ref(&task.id)).await;
        self.enqueue(ctx, AgentKey { project_id: project.id.clone(), name }).await;
        Ok(record)
    }

    /// Create the controller once the model's pool is known (initial limit: the data set's
    /// `start`, or the conversations of the working context the pool holds, minus one for the
    /// foreground).
    async fn ensure_controller(&self, host: &Host) {
        if lock(&self.inner.controller).is_some() {
            return;
        }
        let info = host.get::<ModelInfo>(model_methods::INFO, json!({})).await.ok();
        let config = self.inner.data.get().scheduler.clone();
        let slots = info.as_ref().map_or(0, |info| info.slots);
        let room = info.as_ref().map_or(0, |info| pool_room(info, config.working_context_tokens));
        if room == 0 && config.start == 0 {
            // The model is not answering yet: keep the queue's own limit (one) and ask again later.
            return;
        }
        let start = if config.start > 0 { config.start } else { room.saturating_sub(1).max(1) };
        tracing::info!(slots, room, start, working = config.working_context_tokens, "agents start limit");
        let controller = Controller::new(settings_of(&config), start);
        {
            let mut model = lock(&self.inner.model);
            model.slots = slots;
            model.room = room;
        }
        let limit = controller.limit;
        let mut slot = lock(&self.inner.controller);
        if slot.is_none() {
            *slot = Some(controller);
            drop(slot);
            lock(&self.inner.scheduler).set_limit(limit);
        }
    }

    /// One step of the concurrency control loop.
    async fn control_tick(&self, ctx: &Ctx) {
        let host = Host::new(ctx.clone());
        self.ensure_controller(&host).await;
        let config = self.inner.data.get().scheduler.clone();
        if let Ok(slots) = host.get::<Value>(model_methods::SLOTS, json!({})).await {
            let mut model = lock(&self.inner.model);
            model.queued = slots.get("queued").and_then(Value::as_u64).unwrap_or(0) as u32;
            model.busy = slots.get("slots").and_then(Value::as_array).map(|all| all.iter().filter(|slot| slot.get("busy").and_then(Value::as_bool) == Some(true)).count() as u32).unwrap_or(0);
            model.hibernated = slots.get("hibernated").and_then(Value::as_array).map(|keys| keys.len() as u32).unwrap_or(0);
            let cost = slots.pointer("/metrics/hibernation");
            let restores = cost.and_then(|cost| cost.get("restores")).and_then(Value::as_u64).unwrap_or(0) as u32;
            let mean = cost.and_then(|cost| cost.get("restore_ms_mean")).and_then(Value::as_f64).unwrap_or(0.0);
            let now = now_ms();
            model.restores.push_back((now, restores, f64::from(restores) * mean));
            while model.restores.front().is_some_and(|(at, _, _)| now.saturating_sub(*at) > config.window_ms) {
                model.restores.pop_front();
            }
        }
        let now = now_ms();
        let window = {
            let mut usage = lock(&self.inner.usage);
            while usage.front().is_some_and(|sample| now.saturating_sub(sample.ts_ms) > config.window_ms) {
                usage.pop_front();
            }
            let scheduler = lock(&self.inner.scheduler);
            let mut window = window_of(&usage, config.window_ms, scheduler.running.len() as u32, scheduler.queued.len() as u32);
            (window.restores, window.restore_share) = lock(&self.inner.model).restores_in(config.window_ms);
            window
        };
        let adjustment = {
            let mut controller = lock(&self.inner.controller);
            let Some(controller) = controller.as_mut() else { return };
            // A new data set's range applies at once; otherwise one control step.
            controller.update_settings(now, settings_of(&config)).or_else(|| controller.tick(now, &window))
        };
        let Some(adjustment) = adjustment else { return };
        let record = json!({
            "ts_ms": now,
            "from": adjustment.from,
            "to": adjustment.to,
            "reason": adjustment.reason,
            "metrics": window_json(&window),
            "model": lock(&self.inner.model).to_json(),
        });
        tracing::info!(from = adjustment.from, to = adjustment.to, reason = %adjustment.reason, "agents in flight changed");
        {
            let mut adjustments = lock(&self.inner.adjustments);
            adjustments.push_back(record.clone());
            while adjustments.len() > 200 {
                adjustments.pop_front();
            }
        }
        ctx.publish(SCHEDULER_TOPIC, record.clone());
        // Journal the change where the delegations that run these agents can see it.
        let running: Vec<AgentKey> = lock(&self.inner.scheduler).running.clone();
        let mut parents = BTreeSet::new();
        for key in running {
            if let Ok(record) = agent(&host, &key.project_id, &key.name).await
                && let Some(session) = record.session_id
                && let Ok(session) = host.session_get(&session).await
                && let Some(parent) = session.parent
            {
                parents.insert(parent);
            }
        }
        for parent in parents {
            let text = format!("Agents in flight: {} -> {} ({})", adjustment.from, adjustment.to, adjustment.reason);
            let _ = host.journal_append(&parent, kinds::NOTE, json!({"text": text, "purpose": "scheduler", "host_message": false, "adjustment": record})).await;
        }
        let start = lock(&self.inner.scheduler).set_limit(adjustment.to);
        for key in start {
            self.start(ctx.clone(), key);
        }
    }

    /// The lead's check-in routine (docs/SCHEDULES-PLAN.md): a wake into MANAGE every
    /// `check_every_ms` that watches the agent's task; the scheduler merges it into the lead's
    /// routine when there is one and ends it when the watched work is finished. Without a
    /// scheduler this plugin checks on the lead itself ([`Self::start_checks`]).
    async fn check_in(&self, host: &Host, project_id: &str, lead: &str, task_id: &str, mode: &str) {
        let every = self.inner.data.get().limits.check_every_ms;
        if every == 0 {
            return;
        }
        let when = if every % 60_000 == 0 { format!("every {}m", every / 60_000) } else { format!("every {}s", every.div_ceil(1000)) };
        let params = cc_protocol::contracts::schedule::AddParams {
            scope: Some(project_id.to_owned()),
            kind: Some("wake".to_owned()),
            when,
            session_id: Some(lead.to_owned()),
            watches: vec![task_id.to_owned()],
            manage: true,
            mode: Some(mode.to_owned()),
            by: Some("agents".to_owned()),
            ..Default::default()
        };
        let added = match serde_json::to_value(&params) {
            Ok(value) => host.call(cc_protocol::contracts::schedule::methods::ADD, value).await.map(|_| ()),
            Err(error) => Err(RpcError::internal(error.to_string())),
        };
        if let Err(error) = added {
            tracing::info!(%error, lead, "no check-in routine; this plugin checks on the lead itself");
            lock(&self.inner.self_checked).insert((project_id.to_owned(), lead.to_owned()));
        }
    }

    /// Every `check_every_ms` while agents run or wait, the sessions that started them and have no
    /// check-in routine check on them ([`events::CHECK`]).
    fn start_checks(&self, ctx: &Ctx) {
        let this = self.clone();
        let ctx = ctx.clone();
        let park = self.inner.park.clone();
        self.inner.tasks.spawn(async move {
            loop {
                let every = this.inner.data.get().limits.check_every_ms;
                let wait = Duration::from_millis(if every == 0 { 60_000 } else { every });
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = park.cancelled() => return,
                }
                if every > 0 {
                    this.check_leads(&ctx).await;
                }
            }
        });
    }

    async fn check_leads(&self, ctx: &Ctx) {
        let host = Host::new(ctx.clone());
        let keys: Vec<AgentKey> = {
            let scheduler = lock(&self.inner.scheduler);
            scheduler.running.iter().chain(scheduler.queued.iter()).cloned().collect()
        };
        let mut leads = BTreeSet::new();
        for key in keys {
            if let Ok(record) = agent(&host, &key.project_id, &key.name).await
                && let Some(lead) = lead_session(&host, &record).await
            {
                leads.insert((key.project_id.clone(), lead));
            }
        }
        let self_checked = lock(&self.inner.self_checked).clone();
        for (project_id, lead) in leads {
            if self_checked.contains(&(project_id.clone(), lead.clone())) {
                supervise(ctx, Some(&lead), event_of(&project_id, events::CHECK, "", None, ""));
            }
        }
    }

    fn start_control(&self, ctx: &Ctx) {
        let this = self.clone();
        let ctx = ctx.clone();
        let park = self.inner.park.clone();
        self.inner.tasks.spawn(async move {
            loop {
                let tick = Duration::from_millis(this.inner.data.get().scheduler.tick_ms.max(50));
                tokio::select! {
                    _ = tokio::time::sleep(tick) => {}
                    _ = park.cancelled() => return,
                }
                this.control_tick(&ctx).await;
            }
        });
    }

    async fn enqueue(&self, ctx: Ctx, key: AgentKey) {
        self.ensure_controller(&Host::new(ctx.clone())).await;
        let start = lock(&self.inner.scheduler).enqueue(key);
        for key in start {
            self.start(ctx.clone(), key);
        }
        self.place_queued(ctx).await;
    }

    /// Agents waiting for this computer's model start on a joined computer's free model instead
    /// (`mesh.place`, the host's decision; without the mesh plugin nothing changes).
    async fn place_queued(&self, ctx: Ctx) {
        let host = Host::new(ctx.clone());
        let queued: Vec<AgentKey> = lock(&self.inner.scheduler).queued.iter().cloned().collect();
        for key in queued {
            let Ok(record) = agent(&host, &key.project_id, &key.name).await else { continue };
            let Some(session_id) = record.session_id else { continue };
            if self.place_agent(&host, &session_id, true).await.is_some() {
                let start = lock(&self.inner.scheduler).start_remote(&key);
                self.start(ctx.clone(), key);
                for key in start {
                    self.start(ctx.clone(), key);
                }
            }
        }
    }

    /// Where an agent's model calls run: a joined computer's free model when one is (returns its
    /// name), else here (None). `remote_only`: this computer's queue is full, so here is not an
    /// option now.
    async fn place_agent(&self, host: &Host, session_id: &str, remote_only: bool) -> Option<String> {
        // The session's flow is the tag of the models it may use.
        let tag = host.session_get(session_id).await.map(|session| session.flow).unwrap_or_else(|_| flows::default_flow());
        let placement = host.call(mesh_methods::PLACE, json!({"session_id": session_id, "tag": tag, "wait": false, "remote_only": remote_only})).await.ok()?;
        placement.get("node").and_then(Value::as_str)?;
        Some(placement.get("name").and_then(Value::as_str).unwrap_or_default().to_owned())
    }

    fn cancellation(&self, key: &AgentKey) -> CancellationToken {
        lock(&self.inner.cancellations)
            .entry(key.clone())
            .or_insert_with(|| AgentCancellations { controller: self.inner.park.child_token(), manual: Vec::new() })
            .controller.clone()
    }

    /// Explicit Verify/Merge requests can recover stopped work without reviving its controller.
    fn manual_cancellation(&self, key: &AgentKey) -> CancellationToken {
        let mut scopes = lock(&self.inner.cancellations);
        let scope = scopes.entry(key.clone()).or_insert_with(|| AgentCancellations {
            controller: self.inner.park.child_token(), manual: Vec::new(),
        });
        scope.manual.retain(|token| !token.is_cancelled());
        let token = self.inner.park.child_token();
        scope.manual.push(token.clone());
        token
    }

    fn cancel_agent(&self, key: &AgentKey) {
        let mut scopes = lock(&self.inner.cancellations);
        let scope = scopes.entry(key.clone()).or_insert_with(|| AgentCancellations {
            controller: self.inner.park.child_token(), manual: Vec::new(),
        });
        scope.controller.cancel();
        for token in scope.manual.drain(..) {
            token.cancel();
        }
    }

    fn start(&self, ctx: Ctx, key: AgentKey) {
        let token = self.cancellation(&key);
        let this = self.clone();
        self.inner.tasks.spawn(async move {
            let parked = token.clone();
            tokio::select! {
                biased;
                _ = parked.cancelled() => {},
                outcome = this.run_agent(ctx.clone(), key.clone(), token.clone()) => {
                    if let Err(error) = outcome
                        && error.code != codes::CANCELLED
                    {
                        tracing::warn!(agent = %key.name, %error, "agent run ended with an error");
                        let host = Host::with_cancel(ctx.clone(), token.clone());
                        if let Ok(mut record) = agent(&host, &key.project_id, &key.name).await
                            && record.status == "working"
                        {
                            record.status = "blocked".into();
                            record.tasks_failed += 1;
                            let task_id = record.task_id.clone();
                            let record = upsert(&ctx, &host, record).await.ok();
                            let mut failed = None;
                            if let Some(task_id) = task_id {
                                let _ = update_task(&host, json!({"id": task_id, "status": "failed", "result": error.to_string()})).await;
                                failed = task(&host, &task_id).await.ok();
                            }
                            let text = this.inner.data.get().text("board_error", &[("error", &error.to_string())]);
                            board(&host, &key.project_id, "blocker", &key.name, &text, &[]).await;
                            if let Some(record) = &record {
                                let lead = lead_session(&host, record).await;
                                supervise(&ctx, lead.as_deref(), event_of(&key.project_id, events::BLOCKED, &key.name, failed.as_ref(), &text));
                            }
                        }
                    }
                }
            }
            // Stopping frees the scheduler slot too; only plugin parking prevents new starts.
            let next = lock(&this.inner.scheduler).finished(&key);
            if !this.inner.park.is_cancelled() {
                for key in next {
                    this.start(ctx.clone(), key);
                }
                this.place_queued(ctx).await;
            }
        });
    }

    /// Drive one agent: its run in the project's tree, the steward's verification, then dependents.
    async fn run_agent(&self, ctx: Ctx, key: AgentKey, cancel: CancellationToken) -> Result<(), RpcError> {
        let host = Host::with_cancel(ctx.clone(), cancel.clone());
        let record = agent(&host, &key.project_id, &key.name).await?;
        if record.status != "working" {
            return Ok(());
        }
        let (Some(session_id), Some(task_id)) = (record.session_id.clone(), record.task_id.clone()) else {
            return Err(RpcError::internal(format!("agent {} has no session or task", key.name)));
        };
        let effort = lock(&self.inner.efforts).get(&task_id).cloned().unwrap_or_else(|| "medium".into());
        // Its model calls: here when this computer's model is free, else a joined computer's free
        // model (then its place here goes to the next queued agent).
        let already_remote = lock(&self.inner.scheduler).remote.contains(&key);
        if !already_remote
            && let Some(name) = self.place_agent(&host, &session_id, false).await
        {
            tracing::info!(agent = %key.name, on = %name, "the agent's model calls run on a joined computer");
            let start = lock(&self.inner.scheduler).start_remote(&key);
            for key in start {
                self.start(ctx.clone(), key);
            }
        }
        let result = call_agent_run(&host, &session_id, &task_id, &effort, &record.mode, &cancel).await;
        let _ = host.call(mesh_methods::PLACE_END, json!({"session_id": session_id})).await;
        let result = result?;
        let task = task(&host, &task_id).await?;
        match result.outcome.as_str() {
            "done" => {
                if let Stewarded::Verified = self.steward(&ctx, &host, &record, &task, &result, false).await? {
                    self.start_dependents(&ctx, &host, &task, &session_id, &record.mode).await;
                }
                Ok(())
            }
            "cancelled" => Ok(()),
            other => {
                let mut record = record;
                record.status = "blocked".into();
                record.tasks_failed += 1;
                let record = upsert(&ctx, &host, record).await?;
                // Checks the host cannot decide leave the task INDETERMINATE (for a person to
                // judge), not failed.
                let status = if task.status == "indeterminate" { "indeterminate" } else { "failed" };
                update_task(&host, json!({"id": task.id, "status": status, "result": result.summary})).await?;
                let text = if result.summary.trim().is_empty() { self.inner.data.get().text("board_run_ended", &[("outcome", other)]) } else { result.summary.clone() };
                board(&host, &key.project_id, "blocker", &key.name, &text, std::slice::from_ref(&task.id)).await;
                handoff_ended(&host, &record, &task, &format!("ended without finishing ({other})"), &result.summary).await;
                let lead = lead_session(&host, &record).await;
                supervise(&ctx, lead.as_deref(), event_of(&key.project_id, events::BLOCKED, &key.name, Some(&task), &text));
                Ok(())
            }
        }
    }

    // ---- merge steward ----------------------------------------------------------------------

    async fn steward(&self, ctx: &Ctx, host: &Host, record: &AgentRecord, task: &Task, result: &AgentRunResult, allow_stopped: bool) -> Result<Stewarded, RpcError> {
        let _merging = tokio::select! {
            biased;
            _ = host.cancelled() => return Err(RpcError::cancelled()),
            merging = self.inner.merge.lock() => merging,
        };
        host.check_cancelled()?;
        // The run's record may predate a stop while it waited for the merge steward.
        let fresh = agent(host, &record.project_id, &record.name).await?;
        if fresh.status == "stopped" && !allow_stopped {
            return Err(RpcError::cancelled());
        }
        let project = host.project_get(&record.project_id).await?;
        let session_id = record.session_id.clone().unwrap_or_default();
        host.check_cancelled()?;
        // The agent's work is in the project's tree as it happens; done means the task's frozen
        // contract passes there now.
        frozen_intact(task)?;
        let (report, outputs) = contract::verify(host, &task.id, &project.root, &task.contract, &CancellationToken::new()).await?;
        record_verification(host, task, &report, &[&session_id]).await;
        let parent = host.session_get(&session_id).await.ok().and_then(|session| session.parent);
        if let Some(parent) = &parent {
            let _ = host.journal_append(parent, kinds::VERIFICATION, json!({"task_id": task.id, "report": report, "agent": record.name})).await;
        }
        host.check_cancelled()?;
        let fresh = agent(host, &record.project_id, &record.name).await?;
        if fresh.status == "stopped" && !allow_stopped {
            return Err(RpcError::cancelled());
        }
        let mut updated = fresh;
        if report.ok || no_worse_and_awaited(host, &session_id, task, &report, &outputs, &result.changed_files, &project.root).await {
            update_task(host, json!({"id": task.id, "status": "done", "result": format!("verified in the project: {}", result.summary)})).await?;
            updated.status = "done".into();
            updated.tasks_done += 1;
            let updated = upsert(ctx, host, updated).await?;
            let data = self.inner.data.get();
            // What the work left, from the project itself: a file it made and then deleted is named
            // as removed (seen: a handoff listing a file that was gone, and the next agent looking
            // for it nine times).
            let (kept, removed): (Vec<&String>, Vec<&String>) = result.changed_files.iter().partition(|file| std::path::Path::new(&project.root).join(file).exists());
            let mut changed = String::new();
            if !kept.is_empty() {
                changed.push_str(&data.text("board_changed", &[("files", &kept.iter().map(|file| file.as_str()).collect::<Vec<_>>().join(", "))]));
            }
            if !removed.is_empty() {
                changed.push_str(&data.text("board_removed", &[("files", &removed.iter().map(|file| file.as_str()).collect::<Vec<_>>().join(", "))]));
            }
            let post = data.text("board_done", &[("title", &task.title), ("summary", result.summary.trim()), ("changed", &changed)]);
            board(host, &project.id, "handoff", &record.name, &post, std::slice::from_ref(&task.id)).await;
            handoff_ended(host, &updated, task, "is done", &result.summary).await;
            let lead = lead_session(host, &updated).await;
            progress(host, lead.as_deref(), &data.text("progress_done", &[("agent", &record.name), ("title", &task.title)]), &updated).await;
            let _ = host.call(memory_methods::REFRESH, json!({"project_id": project.id})).await;
            Ok(Stewarded::Verified)
        } else {
            update_task(host, json!({"id": task.id, "status": "failed", "result": "the contract fails in the project"})).await?;
            updated.status = "blocked".into();
            updated.tasks_failed += 1;
            let updated = upsert(ctx, host, updated).await?;
            let post = self.inner.data.get().text("board_failing", &[("title", &task.title)]);
            board(host, &project.id, "blocker", &record.name, &post, std::slice::from_ref(&task.id)).await;
            let failing: Vec<String> = report.checks.iter().filter(|check| !check.ok).map(|check| check.name.clone()).collect();
            let tests = report.checks.iter().filter_map(|check| check.failed_tests.clone()).reduce(|mut all, more| {
                all.extend(more);
                all
            });
            let text = format!("{post} Failing: {}.", failing.join(", "));
            let lead = lead_session(host, &updated).await;
            supervise(ctx, lead.as_deref(), Supervise { failed_tests: tests, ..event_of(&project.id, events::BLOCKED, &record.name, Some(task), &text) });
            Ok(Stewarded::Failed)
        }
    }

    /// Start agents for tasks of the same tree whose dependencies are all done.
    async fn start_dependents(&self, ctx: &Ctx, host: &Host, finished: &Task, agent_session: &str, mode: &str) {
        let Some(parent_id) = finished.parent_id.clone() else { return };
        let Ok(list) = host.get::<TaskListResult>(memory_methods::TASK_LIST, json!({"project_id": finished.project_id, "parent_id": parent_id})).await else {
            return;
        };
        let status: HashMap<String, String> = list.tasks.iter().map(|task| (task.id.clone(), task.status.clone())).collect();
        let from = host.session_get(agent_session).await.ok().and_then(|session| session.parent);
        let mut all_done = true;
        for task in &list.tasks {
            if task.status != "done" {
                all_done = false;
            }
            let ready = task.status == "pending"
                && task.assignee.is_none()
                && task.depends_on.iter().all(|dependency| status.get(dependency).map(String::as_str) == Some("done"));
            if host.check_cancelled().is_err() {
                return;
            }
            if ready {
                let spawned = tokio::select! {
                    biased;
                    _ = host.cancelled() => return,
                    spawned = self.spawn(ctx.clone(), SpawnParams {
                        project_id: task.project_id.clone(),
                        task_id: task.id.clone(),
                        name: None,
                        role: None,
                        from_session: from.clone(),
                        mode: mode.to_owned(),
                        flow: None,
                    }) => spawned,
                };
                if let Err(error) = spawned {
                    tracing::warn!(%error, task = %task.id, "could not start a dependent task");
                }
            }
        }
        if all_done && host.check_cancelled().is_ok() {
            let _ = update_task(host, json!({"id": parent_id, "status": "done", "result": "every subtask merged and verified"})).await;
            if let Some(from) = &from {
                let _ = host.journal_append(from, kinds::NOTE, json!({"text": "Every delegated task is merged and verified.", "purpose": "delegation_done", "task_id": parent_id})).await;
            }
            let root = task(host, &parent_id).await.ok();
            supervise(ctx, from.as_deref(), event_of(&finished.project_id, events::TREE_DONE, "", root.as_ref(), ""));
        }
    }

    // ---- verify / merge / steer / stop / list / scheduler -----------------------------------

    async fn verify(&self, ctx: Ctx, params: VerifyParams) -> Result<VerifyReport, RpcError> {
        let host = Host::new(ctx);
        let task = task(&host, &params.task_id).await?;
        let project = host.project_get(&task.project_id).await?;
        let cancel = task.assignee.as_ref().map(|name| self.manual_cancellation(&AgentKey {
            project_id: project.id.clone(), name: name.clone(),
        }));
        // Finishing an explicit operation releases its scope; Stop also reaches it while active.
        let _operation = cancel.clone().map(CancellationToken::drop_guard);
        let host = match cancel {
            Some(cancel) => Host::with_cancel(host.ctx().clone(), cancel),
            None => host,
        };
        let workspace = match params.workspace.clone() {
            Some(workspace) => workspace,
            None => match &task.assignee {
                Some(name) => agent(&host, &project.id, name).await.ok().and_then(|agent| agent.workspace).unwrap_or_else(|| project.root.clone()),
                None => project.root.clone(),
            },
        };
        frozen_intact(&task)?;
        update_task(&host, json!({"id": task.id, "status": "verifying"})).await?;
        let (report, _) = contract::verify(&host, &task.id, &workspace, &task.contract, &CancellationToken::new()).await?;
        let sessions: Vec<&str> = task.session_id.iter().map(String::as_str).collect();
        record_verification(&host, &task, &report, &sessions).await;
        // Verified in the project root: done. Verified in an agent's workspace: waiting for the
        // merge steward, which verifies again after merging.
        let undecided = report.checks.iter().any(|check| check.indeterminate);
        let fixable = report.checks.iter().any(|check| !check.ok && !check.indeterminate);
        let status = match (report.ok, workspace == project.root) {
            (true, true) => "done",
            (true, false) => "verifying",
            // Only checks the host cannot decide are left: a person judges them, not the worker.
            (false, _) if undecided && !fixable => "indeterminate",
            (false, _) => "in_progress",
        };
        let passed = report.checks.iter().filter(|check| check.ok).count();
        update_task(&host, json!({"id": task.id, "status": status, "result": format!("{passed}/{} checks pass in {workspace}", report.checks.len())})).await?;
        Ok(report)
    }

    async fn merge(&self, ctx: Ctx, params: MergeParams) -> Result<MergeOutcome, RpcError> {
        let host = Host::new(ctx.clone());
        let record = agent(&host, &params.project_id, &params.agent).await?;
        let cancel = self.manual_cancellation(&AgentKey { project_id: params.project_id.clone(), name: params.agent.clone() });
        let _operation = cancel.clone().drop_guard();
        let host = Host::with_cancel(ctx.clone(), cancel);
        let task_id = record.task_id.clone().ok_or_else(|| RpcError::invalid_params("the agent has no task"))?;
        let task = task(&host, &task_id).await?;
        let result = AgentRunResult { outcome: "done".into(), summary: String::new(), turns: 0, tool_calls: 0, changed_files: vec![], usage: Value::Null };
        let outcome = self.steward(&ctx, &host, &record, &task, &result, true).await?;
        Ok(match outcome {
            Stewarded::Verified => {
                // A task verified later releases the tasks that depend on it.
                if let Some(session_id) = &record.session_id {
                    self.start_dependents(&ctx, &host, &task, session_id, &record.mode).await;
                }
                MergeOutcome { ok: true, conflicts: vec![], sha: None, detail: "verified in the project".into() }
            }
            Stewarded::Failed => MergeOutcome { ok: false, conflicts: vec![], sha: None, detail: "the contract fails in the project".into() },
        })
    }

    async fn steer(&self, ctx: Ctx, params: AgentSteerParams) -> Result<Value, RpcError> {
        let host = Host::new(ctx);
        let record = agent(&host, &params.project_id, &params.agent).await?;
        let session_id = record.session_id.ok_or_else(|| RpcError::invalid_params("the agent has no session"))?;
        host.call(&flow_method(&host, &session_id, chatflow_verbs::STEER).await, forwarded_steer(&session_id, &params)).await
    }

    /// Stop an agent: its run ends, its task is cancelled with the tasks waiting for it (they would
    /// never start), and what it already changed stays in the files.
    async fn stop(&self, ctx: Ctx, params: AgentRefParams) -> Result<Value, RpcError> {
        let host = Host::new(ctx.clone());
        let record = agent(&host, &params.project_id, &params.agent).await?;
        let key = AgentKey { project_id: params.project_id.clone(), name: params.agent.clone() };
        let was_queued = lock(&self.inner.scheduler).dequeue(&key);
        let mut result = CancelResult::default();
        match &record.task_id {
            Some(task_id) => {
                let first = task(&host, task_id).await?;
                self.cancel_tree(&ctx, &host, &first, &mut result).await?;
            }
            None => {
                self.halt(&ctx, &host, record).await?;
            }
        }
        Ok(json!({"stopped": true, "was_queued": was_queued, "cancelled": result.tasks, "agents": result.agents}))
    }

    /// Cancel a task with its subtasks and the tasks waiting for it; their agents stop.
    async fn cancel(&self, ctx: Ctx, params: CancelParams) -> Result<CancelResult, RpcError> {
        let host = Host::new(ctx.clone());
        let first = task(&host, &params.task_id).await?;
        if first.project_id != params.project_id {
            return Err(RpcError::invalid_params("the task belongs to another project"));
        }
        let mut result = CancelResult::default();
        self.cancel_tree(&ctx, &host, &first, &mut result).await?;
        Ok(result)
    }

    /// End an agent's run (its scheduler place, its model call) and record it stopped.
    async fn halt(&self, ctx: &Ctx, host: &Host, mut record: AgentRecord) -> Result<AgentRecord, RpcError> {
        let key = AgentKey { project_id: record.project_id.clone(), name: record.name.clone() };
        // The run is cancelled while its caller is still there: the controller's end drops the
        // `agent_run` call, and a run whose caller went away parks (no end status, so it would
        // still read as running).
        if let Some(session_id) = &record.session_id {
            host.call(&flow_method(host, session_id, chatflow_verbs::CANCEL).await, json!({"session_id": session_id})).await?;
        }
        self.cancel_agent(&key);
        lock(&self.inner.scheduler).dequeue(&key);
        if record.status != "done" {
            record.status = "stopped".into();
        }
        upsert(ctx, host, record).await
    }

    /// `first` and everything that hangs on it: its subtasks and, in its tree, the unfinished tasks
    /// that depend on a cancelled one. Done tasks stay done.
    async fn cancel_tree(&self, ctx: &Ctx, host: &Host, first: &Task, result: &mut CancelResult) -> Result<(), RpcError> {
        let data = self.inner.data.get();
        let mut stack = vec![first.clone()];
        let mut seen = BTreeSet::new();
        let mut lead = None;
        let mut parents = BTreeSet::new();
        while let Some(current) = stack.pop() {
            if !seen.insert(current.id.clone()) || finished(&current.status) {
                continue;
            }
            if let Some(name) = &current.assignee
                && let Ok(record) = agent(host, &current.project_id, name).await
                && record.task_id.as_deref() == Some(current.id.as_str())
                && record.status != "done"
            {
                if lead.is_none() {
                    lead = lead_session(host, &record).await;
                }
                let record = self.halt(ctx, host, record).await?;
                let stopped = data.text("progress_cancelled_agent", &[("agent", &record.name)]);
                result.agents.push(record.name.clone());
                progress(host, lead.as_deref(), &data.text("progress_cancelled", &[("title", &current.title), ("agent", &stopped)]), &record).await;
            }
            update_task(host, json!({"id": current.id, "status": "cancelled"})).await?;
            parents.extend(current.parent_id.clone());
            let author = current.assignee.clone().unwrap_or_else(|| "lead".into());
            board(host, &current.project_id, "status", &author, &data.text("board_cancelled", &[("title", &current.title)]), std::slice::from_ref(&current.id)).await;
            result.tasks.push(current.id.clone());
            if let Ok(children) = host.get::<TaskListResult>(memory_methods::TASK_LIST, json!({"project_id": current.project_id, "parent_id": current.id})).await {
                stack.extend(children.tasks);
            }
            if let Some(parent) = &current.parent_id
                && let Ok(siblings) = host.get::<TaskListResult>(memory_methods::TASK_LIST, json!({"project_id": current.project_id, "parent_id": parent})).await
            {
                stack.extend(siblings.tasks.into_iter().filter(|task| task.depends_on.contains(&current.id)));
            }
        }
        // A tree whose every task has finished, some of them cancelled, is cancelled itself (its
        // summary would otherwise wait for ever).
        for parent in parents {
            let Ok(children) = host.get::<TaskListResult>(memory_methods::TASK_LIST, json!({"project_id": first.project_id, "parent_id": parent})).await else { continue };
            if children.tasks.iter().all(|task| finished(&task.status))
                && let Ok(root) = task(host, &parent).await
                && !finished(&root.status)
            {
                update_task(host, json!({"id": parent, "status": "cancelled"})).await?;
            }
        }
        Ok(())
    }

    /// Tasks cancelled only because they waited for `task` (never claimed) are pending again.
    async fn reopen_dependents(&self, host: &Host, task: &Task) {
        let Some(parent) = &task.parent_id else { return };
        let Ok(siblings) = host.get::<TaskListResult>(memory_methods::TASK_LIST, json!({"project_id": task.project_id, "parent_id": parent})).await else { return };
        let mut reopened = vec![task.id.clone()];
        let mut changed = true;
        while changed {
            changed = false;
            for sibling in &siblings.tasks {
                if sibling.status == "cancelled"
                    && sibling.assignee.is_none()
                    && !reopened.contains(&sibling.id)
                    && sibling.depends_on.iter().any(|dependency| reopened.contains(dependency))
                {
                    let _ = update_task(host, json!({"id": sibling.id, "status": "pending"})).await;
                    reopened.push(sibling.id.clone());
                    changed = true;
                }
            }
        }
    }

    /// A board post: an agent's need addressed to its lead tells the session that delegated it.
    async fn board_posted(&self, ctx: Ctx, data: Value) -> Result<(), RpcError> {
        let Some(message) = data.get("message").cloned().and_then(|message| serde_json::from_value::<BoardMessage>(message).ok()) else {
            return Ok(());
        };
        if message.kind != "need" || message.to.as_deref() != Some("lead") {
            return Ok(());
        }
        let host = Host::new(ctx.clone());
        let Ok(record) = agent(&host, &message.project_id, &message.author).await else { return Ok(()) };
        let current = match &record.task_id {
            Some(task_id) => task(&host, task_id).await.ok(),
            None => None,
        };
        let lead = lead_session(&host, &record).await;
        supervise(&ctx, lead.as_deref(), event_of(&message.project_id, events::NEED, &record.name, current.as_ref(), &message.text));
        Ok(())
    }

    /// A blocked or stopped agent back on its task, in its own session. A run cut off by an error
    /// is still `running` in the journal and resumes as it was; an ended run gets a follow-up, so
    /// the chat flow starts a new run that continues from the work already in the session.
    async fn restart(&self, ctx: Ctx, params: AgentRefParams) -> Result<AgentRecord, RpcError> {
        let host = Host::new(ctx.clone());
        let mut record = agent(&host, &params.project_id, &params.agent).await?;
        if record.status != "blocked" && record.status != "stopped" {
            return Err(RpcError::invalid_params(format!("{} is {}; only a blocked or stopped agent restarts", record.name, record.status)));
        }
        let (Some(session_id), Some(task_id)) = (record.session_id.clone(), record.task_id.clone()) else {
            return Err(RpcError::invalid_params(format!("{} has no session or task to go back to", record.name)));
        };
        let entries = host.journal_read(&session_id, 1).await?;
        let cut_off = entries
            .iter()
            .rev()
            .find(|entry| entry.kind == kinds::STATUS)
            .is_some_and(|status| status.data.get("state").and_then(Value::as_str) == Some("running"));
        if !cut_off {
            let text = self.inner.data.get().text("restart_followup", &[]);
            host.journal_append(&session_id, kinds::STEER, json!({"text": text, "kind": "followup", "target": "chat", "consumed": false})).await?;
        }
        // A stop cancelled the old scope for good; the restarted run gets a fresh one.
        let key = AgentKey { project_id: record.project_id.clone(), name: record.name.clone() };
        lock(&self.inner.cancellations).remove(&key);
        record.status = "working".into();
        let record = upsert(&ctx, &host, record).await?;
        let task = task(&host, &task_id).await?;
        update_task(&host, json!({"id": task.id, "status": "in_progress"})).await?;
        // The tasks its stop cancelled because they waited for it wait again.
        self.reopen_dependents(&host, &task).await;
        let text = self.inner.data.get().text("board_restart", &[("title", &task.title)]);
        board(&host, &record.project_id, "status", &record.name, &text, std::slice::from_ref(&task.id)).await;
        let lead = lead_session(&host, &record).await;
        progress(&host, lead.as_deref(), &self.inner.data.get().text("progress_restarted", &[("agent", &record.name), ("title", &task.title)]), &record).await;
        self.enqueue(ctx, key).await;
        Ok(record)
    }

    fn scheduler_state(&self) -> SchedulerState {
        let config = self.inner.data.get().scheduler.clone();
        let scheduler = lock(&self.inner.scheduler);
        let window = {
            let usage = lock(&self.inner.usage);
            let mut window = window_of(&usage, config.window_ms, scheduler.running.len() as u32, scheduler.queued.len() as u32);
            (window.restores, window.restore_share) = lock(&self.inner.model).restores_in(config.window_ms);
            window
        };
        let controller = lock(&self.inner.controller);
        let adjustments: Vec<Value> = lock(&self.inner.adjustments).iter().cloned().collect();
        SchedulerState {
            running: scheduler.running.iter().map(AgentKey::label).collect(),
            queued: scheduler.queued.iter().map(AgentKey::label).collect(),
            max_parallel: scheduler.limit,
            detail: json!({
                "active": scheduler.running.len(),
                "queued": scheduler.queued.len(),
                "limit": scheduler.limit,
                "why": adjustments.last().and_then(|last| last.get("reason").cloned()).unwrap_or_else(|| json!("initial limit")),
                "bounds": {"min": config.min, "max": config.max, "ceiling": controller.as_ref().and_then(|c| c.ceiling)},
                "peak": scheduler.peak,
                "model": lock(&self.inner.model).to_json(),
                "metrics": window_json(&window),
                "throughput_by_limit": controller.as_ref().map(|c| c.samples.iter().map(|(limit, tps)| json!({"limit": limit, "tokens_per_s": tps})).collect::<Vec<_>>()).unwrap_or_default(),
                "adjustments": adjustments,
            }),
        }
    }

    /// A worker's proposed revision of its frozen contract. Refused mechanically when a proposed
    /// check cannot be run in the root or nothing would be machine-checked; otherwise the planner
    /// judges it in a fresh context (never the worker's conversation). An accepted revision is
    /// written with the old hash, the reason and the evidence; memory refuses any other change.
    async fn propose(&self, ctx: Ctx, params: ProposeParams) -> Result<ProposeResult, RpcError> {
        let host = Host::new(ctx);
        let task = task(&host, &params.task_id).await?;
        let project = host.project_get(&task.project_id).await?;
        let data = self.inner.data.get();
        let refused = |reason: String| ProposeResult { accepted: false, reason, contract: task.contract.clone(), contract_hash: task.contract_hash.clone() };
        let workspace = params.workspace.clone().unwrap_or_else(|| project.root.clone());
        let dirs = [workspace.as_str(), project.root.as_str()];
        let proposed = Contract {
            checks: params
                .contract
                .checks
                .iter()
                .map(|check| ContractCheck { spec: if check.kind == "command" { contract::rooted(&check.spec, &dirs) } else { check.spec.clone() }, ..check.clone() })
                .collect(),
        };
        // Mechanical refusals first: they never reach the planner, but are journaled like any outcome.
        let mechanical = if proposed.hash() == task.contract.hash() {
            Some(data.text("review_refused_unchanged", &[]))
        } else if !proposed.checks.iter().any(|check| check.kind == "command") {
            Some(data.text("review_refused_empty", &[]))
        } else {
            proposed
                .checks
                .iter()
                .filter(|check| check.kind == "command")
                .flat_map(|check| [workspace.as_str(), project.root.as_str()].map(|root| contract::refusal(&check.spec, root)))
                .flatten()
                .next()
                .map(|why| data.text("review_refused_check", &[("why", &why)]))
        };
        let session = match params.session_id.clone() {
            Some(session) => session,
            None => {
                let worker = task.session_id.clone().unwrap_or_default();
                host.session_get(&worker).await.ok().and_then(|session| session.parent).unwrap_or(worker)
            }
        };
        let mut journal = Journal::load(host.clone(), &session).await?;
        let run_id = format!("review-{}", uuid::Uuid::new_v4().simple());
        let request = data.text(
            "review_request",
            &[
                ("title", task.title.trim()),
                ("brief", task.brief.trim()),
                ("old", &checks_text(&data, &task.contract)),
                ("worker", params.proposed_by.trim()),
                ("new", &checks_text(&data, &proposed)),
                ("evidence", params.evidence.trim()),
            ],
        );
        let mut live = String::new();
        let mut review: Option<Review> = None;
        let attempts = if mechanical.is_some() { 0 } else { data.plan_attempts };
        for _attempt in 0..attempts {
            let stage = if live.is_empty() { data.text("stage", &[("stage", "REVIEW")]) } else { data.text("stage_live", &[("stage", "REVIEW"), ("live", &live)]) };
            let call = CallSpec {
                stage: "REVIEW".into(),
                messages: vec![
                    ChatMessage::new("system", data.review_system()),
                    ChatMessage::new("user", request.clone()),
                    ChatMessage::new("user", format!("{} {stage}", data.text("host_prefix", &[]))),
                ],
                tools: Vec::new(),
                json_schema: None,
                grammar: Some(schemas::grammar(data.review_shape())),
                tool_choice: None,
                effort: data.plan_effort().to_owned(),
                sampling: data.plan_sampling(data.plan_effort() != "none"),
                priority: Priority::Foreground,
                cache_key: format!("{}:review", task.id),
                stream: StreamTo { run_id: run_id.clone(), step: 1, content: false },
            };
            let called = llm::chat(&mut journal, call, &CancellationToken::new()).await?;
            match parse_json::<Review>(&called.result.content) {
                Ok(decided) if decided.decision == "accept" || decided.decision == "refuse" => {
                    review = Some(decided);
                    break;
                }
                Ok(decided) => live = data.text("review_unreadable", &[("error", &format!("unknown decision `{}`", decided.decision))]),
                Err(error) => live = data.text("review_unreadable", &[("error", &error)]),
            }
        }
        let old_hash = task.contract_hash.clone().unwrap_or_else(|| task.contract.hash());
        let outcome = match review {
            _ if mechanical.is_some() => refused(mechanical.clone().unwrap_or_default()),
            Some(review) if review.decision == "accept" => {
                let updated: Task = host
                    .get(
                        memory_methods::TASK_UPDATE,
                        json!({
                            "id": task.id,
                            "contract": proposed,
                            "freeze": true,
                            "revision": {
                                "old_hash": old_hash,
                                "reason": review.reason,
                                "evidence": params.evidence,
                                "proposed_by": params.proposed_by,
                                "accepted_by": FOREMAN,
                            },
                        }),
                    )
                    .await?;
                ProposeResult { accepted: true, reason: review.reason, contract: updated.contract, contract_hash: updated.contract_hash }
            }
            Some(review) => refused(review.reason),
            None => refused(data.text("review_no_decision", &[])),
        };
        journal
            .append(
                kinds::NOTE,
                json!({
                    "text": format!("Contract revision for \"{}\" proposed by {}: {} ({})", task.title, params.proposed_by, if outcome.accepted { "accepted" } else { "refused" }, outcome.reason),
                    "purpose": "contract_review",
                    "run_id": run_id,
                    "host_message": false,
                    "task_id": task.id,
                    "accepted": outcome.accepted,
                    "reason": outcome.reason,
                    "proposed_by": params.proposed_by,
                    "old_hash": old_hash,
                    "new_hash": proposed.hash(),
                    "mechanical": mechanical.is_some(),
                    "data_version": data.version(),
                }),
            )
            .await?;
        Ok(outcome)
    }

    async fn readopt(&self, ctx: Ctx) -> Result<(), RpcError> {
        let host = Host::new(ctx.clone());
        for project in host.project_list().await? {
            self.open_project(&host, &project).await;
            let Ok(agents) = agents_of(&host, &project.id).await else { continue };
            for record in agents.into_iter().filter(|agent| agent.status == "working") {
                tracing::info!(agent = %record.name, project = %project.id, "re-adopting a working agent");
                if let (Some(lead), Some(task_id)) = (lead_session(&host, &record).await, record.task_id.clone()) {
                    self.check_in(&host, &project.id, &lead, &task_id, &record.mode).await;
                }
                self.enqueue(ctx.clone(), AgentKey { project_id: project.id.clone(), name: record.name }).await;
            }
        }
        Ok(())
    }
}

enum Stewarded {
    Verified,
    Failed,
}

fn pick_name(taken: &BTreeSet<&str>) -> String {
    if let Some(name) = NAMES.iter().find(|name| !taken.contains(**name)) {
        return (*name).to_owned();
    }
    (2..)
        .flat_map(|round| NAMES.iter().map(move |name| format!("{name}{round}")))
        .find(|name| !taken.contains(name.as_str()))
        .unwrap_or_else(|| format!("agent-{}", uuid::Uuid::new_v4().simple()))
}

/// `chatflow.agent_run`, retried while the chat flow restarts (the call resumes from the journal,
/// so repeating it is safe).
async fn call_agent_run(host: &Host, session_id: &str, task_id: &str, effort: &str, mode: &str, cancel: &CancellationToken) -> Result<AgentRunResult, RpcError> {
    let params = json!({"session_id": session_id, "task_id": task_id, "mode": mode, "options": {"effort": effort}});
    let method = flow_method(host, session_id, chatflow_verbs::AGENT_RUN).await;
    let mut delay = Duration::from_millis(250);
    loop {
        host.check_cancelled()?;
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RpcError::cancelled()),
            outcome = host.call_with(&method, params.clone(), host::Effect::Idempotent, Some(cancel)) => outcome,
        };
        match outcome {
            Ok(value) => return parse_params(value),
            Err(error) if error.code == codes::CANCELLED => return Err(error),
            Err(error) if error.retryable || host::refused(&error) || host::interrupted(&error) => {
                tracing::info!(%error, session_id, "agent run interrupted; calling again");
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(RpcError::cancelled()),
                    _ = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(2));
            }
            Err(error) => return Err(error),
        }
    }
}

/// A task's contract is the one frozen at claim (or its accepted revision): anything else was
/// changed behind the rule and is never verified.
fn frozen_intact(task: &Task) -> Result<(), RpcError> {
    match &task.contract_hash {
        Some(frozen) if *frozen != task.contract.hash() => Err(RpcError::internal(format!(
            "the contract of task {} does not match its frozen hash ({frozen}); it was changed without an accepted revision",
            task.id
        ))),
        _ => Ok(()),
    }
}

fn checks_text(data: &Dataset, contract: &Contract) -> String {
    contract.checks.iter().map(|check| data.text("review_check", &[("command", &check.spec)])).collect()
}

/// Whether the work made nothing worse than its run's baseline (the agent session's `baseline`
/// notes: every failed check fails only tests named as failing for the same command before, both
/// known) while a later task of its plan still waits for it: then it is done, and what failed
/// before is the waiting task's work (the plan's last task fixes it). The worker's loop judges
/// the same way.
/// Also done while awaited: every failed check's errors are in files outside the task's own (the
/// files it changed and those its changed signatures reached, the session's `impact` notes):
/// another task's part, which the plan's last task makes pass.
async fn no_worse_and_awaited(host: &Host, session_id: &str, task: &Task, report: &VerifyReport, outputs: &[String], changed: &[String], root: &str) -> bool {
    let failed: Vec<_> = report.checks.iter().filter(|check| !check.ok).collect();
    if failed.is_empty() || session_id.is_empty() {
        return false;
    }
    let Ok(entries) = host.journal_read(session_id, 1).await else { return false };
    let mut own: std::collections::HashSet<String> = changed.iter().map(|path| path.strip_prefix(root).map(|rest| rest.trim_start_matches('/')).unwrap_or(path).trim_start_matches("./").to_owned()).collect();
    for note in entries.iter().filter(|entry| entry.kind == kinds::NOTE && entry.data.get("purpose").and_then(Value::as_str) == Some("impact")) {
        own.extend(note.data.get("reached").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
    }
    let not_its = report.checks.iter().zip(outputs).filter(|(check, _)| !check.ok && !check.indeterminate).all(|(_, output)| {
        error_files(output, root).is_some_and(|files| files.iter().all(|file| !own.contains(file)))
    });
    let no_worse = not_its || failed.iter().all(|check| {
        let (Some(now), Some(spec)) = (&check.failed_tests, report.contract.checks.iter().find(|spec| spec.name == check.name)) else { return false };
        let before = entries
            .iter()
            .filter(|entry| entry.kind == kinds::NOTE && entry.data.get("purpose").and_then(Value::as_str) == Some("baseline"))
            .filter_map(|note| note.data.get("failed_tests")?.get(&spec.spec)?.as_array().cloned())
            .last();
        before.is_some_and(|before| now.iter().all(|name| before.iter().any(|known| known.as_str() == Some(name.as_str()))))
    });
    if !no_worse {
        return false;
    }
    let Some(parent) = &task.parent_id else { return false };
    let Ok(list) = host.get::<TaskListResult>(memory_methods::TASK_LIST, json!({"project_id": task.project_id, "parent_id": parent})).await else { return false };
    list.tasks.iter().any(|other| other.id != task.id && other.depends_on.contains(&task.id) && !matches!(other.status.as_str(), "done" | "cancelled" | "failed"))
}

/// The files a check's output names in its error lines (warnings and notes do not count); None
/// when no error line names a file. The worker's loop reads them the same way
/// (`chatflow::checks::error_files`).
fn error_files(output: &str, workspace: &str) -> Option<std::collections::HashSet<String>> {
    let mut files = std::collections::HashSet::new();
    let mut in_error = false;
    for line in output.lines() {
        let trimmed = line.trim_start();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("error") {
            in_error = true;
        } else if lower.starts_with("warning") || lower.starts_with("note") || lower.starts_with("help") {
            in_error = false;
        }
        let (located, rest) = match trimmed.strip_prefix("--> ") {
            Some(rest) if in_error => (true, rest),
            _ => (false, trimmed),
        };
        let path: String = rest.chars().take_while(|c| !matches!(c, ':' | '(' | ' ')).collect();
        let after = &rest[path.len()..];
        let has_line = (after.starts_with(':') || after.starts_with('(')) && after[1..].starts_with(|c: char| c.is_ascii_digit());
        let file_like = path.rsplit('/').next().is_some_and(|name| name.contains('.') && !name.ends_with('.')) && !path.contains("://");
        if !(has_line && file_like) {
            continue;
        }
        let message = after.to_ascii_lowercase();
        if located || (!message.contains("warning") && !message.contains(" note:")) {
            files.insert(path.strip_prefix(workspace).map(|rest| rest.trim_start_matches('/')).unwrap_or(&path).trim_start_matches("./").to_owned());
        }
    }
    (!files.is_empty()).then_some(files)
}

async fn record_verification(host: &Host, task: &Task, report: &VerifyReport, sessions: &[&str]) {
    for session in sessions.iter().filter(|session| !session.is_empty()) {
        if let Err(error) = host.journal_append(session, kinds::VERIFICATION, json!({"task_id": task.id, "report": report})).await {
            tracing::warn!(%error, "could not journal the verification");
        }
    }
}

/// Graph context for planning, targeted at the goal (no repository map: the planner never gets
/// the whole repository): what the goal resembles by meaning, and what depends on that code.
async fn graph_context(host: &Host, project: &Project, goal: &str, data: &Dataset) -> String {
    let mut parts = Vec::new();
    let search = host.get::<SearchResult>(memory_methods::SEARCH, json!({"project_id": project.id, "text": goal, "limit": data.limits.search_limit})).await;
    if let Ok(search) = &search {
        let related = graph::render(memory_methods::SEARCH, &serde_json::to_value(search).unwrap_or_default());
        parts.push(data.text("graph_related", &[("text", &related)]));
        let sids: Vec<&str> = search.hits.iter().map(|hit| hit.symbol.sid.as_str()).collect();
        if !sids.is_empty()
            && let Ok(impact) = host.get::<ImpactResult>(memory_methods::IMPACT, json!({"project_id": project.id, "sids": sids, "paths": [], "depth": 1})).await
        {
            parts.push(data.text("graph_impact", &[("text", &render_impact(&impact))]));
        }
    }
    if parts.is_empty() { data.text("graph_unavailable", &[]) } else { parts.join("\n\n") }
}

/// The project's candidate checks: the applicable verifiers of the active language plugins
/// (`lang@1` describe), as the chat flow offers them. No files are named before the plan, so a
/// verifier that checks named files is not offered; empty when nothing applies.
async fn verifier_candidates(host: &Host, root: &str) -> Vec<Candidate> {
    let Ok(listed) = host.get::<Value>(cc_protocol::contracts::frame::methods::PLUGINS_LIST, json!({})).await else { return Vec::new() };
    let mut descriptions: Vec<Description> = Vec::new();
    for entry in listed.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default() {
        let field = |name: &str| entry.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        if field("contract") != lang::CONTRACT || field("state") != "active" {
            continue;
        }
        if let Ok(description) = host.get::<Description>(&format!("{}.describe", field("category")), json!({})).await {
            descriptions.push(description);
        }
    }
    verifiers::candidates(std::path::Path::new(root), &descriptions, std::env::var_os("PATH").as_deref(), &[])
}

/// The candidates as the stage message's text; empty when nothing applies.
fn candidates_text(data: &Dataset, candidates: &[Candidate]) -> String {
    if candidates.is_empty() {
        return String::new();
    }
    let list = candidates.iter().map(|candidate| data.text("verifier_line", &[("kind", &candidate.kind), ("command", &candidate.command)])).collect::<Vec<_>>().join("\n");
    data.text("verifiers_candidates", &[("list", &list)])
}

/// The files a task affects: what it touches plus their callers and tests (from the graph).
async fn impact_set(host: &Host, project: &Project, touches: &[String]) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let mut paths = Vec::new();
    let mut sids = Vec::new();
    for touch in touches.iter().map(|touch| touch.trim()).filter(|touch| !touch.is_empty()) {
        // Structural: an existing path is a file; anything else is looked up as a symbol.
        if std::path::Path::new(&project.root).join(touch).exists() {
            let path = touch.trim_start_matches("./").to_owned();
            set.insert(path.clone());
            paths.push(path);
        } else if let Ok(matches) = host.get::<SymbolMatches>(memory_methods::SYMBOL, json!({"project_id": project.id, "name": touch})).await {
            for detail in matches.matches {
                set.insert(detail.symbol.path.clone());
                sids.push(detail.symbol.sid.clone());
            }
        }
    }
    if paths.is_empty() && sids.is_empty() {
        return set;
    }
    if let Ok(impact) = host.get::<ImpactResult>(memory_methods::IMPACT, json!({"project_id": project.id, "sids": sids, "paths": paths, "depth": 1})).await {
        set.extend(impact.changed.iter().map(|symbol| symbol.path.clone()));
        set.extend(impact.call_sites.iter().map(|site| site.path.clone()));
        set.extend(impact.tests.iter().map(|test| test.path.clone()));
    }
    set
}

fn probe_checks(data_dir: &std::path::Path) -> Vec<ProbeCheck> {
    let mut checks = Vec::new();
    let started = Instant::now();
    let plan_ok = {
        let parsed: Result<Plan, String> = parse_json(
            r#"{"tasks":[{"title":"a","brief":"b","plan":"p","checks":["make test"],"touches":["x.py"],"depends_on":[],"effort":"low"},{"title":"c","brief":"b","plan":"p","checks":["make test"],"touches":["x.py"],"depends_on":[],"effort":"low"}]}"#,
        );
        match parsed {
            Ok(plan) => {
                let mut tasks = plan.tasks;
                let impact: Vec<BTreeSet<String>> = tasks.iter().map(|task| task.touches.iter().cloned().collect()).collect();
                let added = plan::serialize(&mut tasks, &impact);
                if plan::validate(&tasks).is_empty() && added.len() == 1 && plan::overlaps(&tasks, &impact).is_empty() {
                    Ok("overlapping tasks were serialized".to_owned())
                } else {
                    Err("the cut did not serialize an overlap".to_owned())
                }
            }
            Err(error) => Err(error),
        }
    };
    let (ok, detail) = match plan_ok {
        Ok(detail) => (true, detail),
        Err(detail) => (false, detail),
    };
    checks.push(ProbeCheck { name: "plan cut".into(), ok, detail, ms: started.elapsed().as_millis() as u64 });
    let started = Instant::now();
    let mut scheduler = Scheduler::new(1);
    let first = scheduler.enqueue(AgentKey { project_id: "p".into(), name: "a".into() });
    let second = scheduler.enqueue(AgentKey { project_id: "p".into(), name: "b".into() });
    let ok = first.len() == 1 && second.is_empty() && scheduler.limit == 1;
    checks.push(ProbeCheck { name: "scheduler".into(), ok, detail: format!("limit {}", scheduler.limit), ms: started.elapsed().as_millis() as u64 });
    let started = Instant::now();
    let ok = contract::expectation_holds(Some(r"\d+ passed"), "3 passed") && !contract::expectation_holds(Some("OK"), "FAILED");
    checks.push(ProbeCheck { name: "contract expectations".into(), ok, detail: String::new(), ms: started.elapsed().as_millis() as u64 });
    let writable = std::fs::create_dir_all(data_dir).and_then(|_| std::fs::write(data_dir.join(".probe"), b"ok"));
    checks.push(ProbeCheck { name: "data dir".into(), ok: writable.is_ok(), detail: format!("{writable:?}"), ms: 0 });
    checks
}

/// Entries of a journal written by `run_id` (used by tests and reports).
pub fn of_run<'a>(entries: &'a [JournalEntry], run_id: &str) -> Vec<&'a JournalEntry> {
    entries.iter().filter(|entry| str_field(&entry.data, "run_id") == run_id).collect()
}

/// `chatflow.steer` params for an agent's session: the text, kind, and (only when given) the
/// attachments and the mentions of other projects.
fn forwarded_steer(session_id: &str, params: &AgentSteerParams) -> Value {
    let mut forwarded = json!({"session_id": session_id, "text": params.text, "kind": params.kind});
    if !params.attachments.is_empty() {
        forwarded["attachments"] = json!(params.attachments);
    }
    if !params.mentions.is_empty() {
        forwarded["mentions"] = json!(params.mentions);
    }
    forwarded
}

impl Plugin for Agents {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let this = self.clone();
        Box::pin(async move {
            let params = call.params.clone();
            match call.method.as_str() {
                methods::DELEGATE => to_value(&this.delegate(ctx, parse_params(params)?).await?),
                methods::SPAWN => to_value(&this.spawn(ctx, parse_params(params)?).await?),
                methods::HANDOFF => to_value(&this.handoff(ctx, parse_params(params)?).await?),
                methods::LIST => {
                    let params: ListParams = parse_params(params)?;
                    to_value(&ListResult { agents: agents_of(&Host::new(ctx), &params.project_id).await? })
                }
                methods::STEER => this.steer(ctx, parse_params(params)?).await,
                methods::STOP => this.stop(ctx, parse_params(params)?).await,
                methods::RESTART => to_value(&this.restart(ctx, parse_params(params)?).await?),
                methods::CANCEL => to_value(&this.cancel(ctx, parse_params(params)?).await?),
                methods::VERIFY => to_value(&this.verify(ctx, parse_params(params)?).await?),
                methods::MERGE => to_value(&this.merge(ctx, parse_params(params)?).await?),
                methods::SCHEDULER => to_value(&this.scheduler_state()),
                methods::PROPOSE => to_value(&this.propose(ctx, parse_params(params)?).await?),
                other => Err(RpcError::unknown_method(other)),
            }
        })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        Box::pin(async move { Ok(ProbeReport::from_checks(probe_checks(&ctx.env.data_dir))) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let this = self.clone();
        Box::pin(async move {
            this.start_data(&ctx);
            this.start_control(&ctx);
            this.start_checks(&ctx);
            tokio::spawn(async move {
                if let Err(error) = this.readopt(ctx).await {
                    tracing::warn!(%error, "could not re-adopt working agents");
                }
            });
            Ok(())
        })
    }

    fn event(&self, ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        if topic == memory_topics::BOARD_POSTED {
            let this = self.clone();
            return Box::pin(async move {
                if let Err(error) = this.board_posted(ctx, data).await {
                    tracing::warn!(%error, "could not pass a board need to its lead");
                }
            });
        }
        if topic == chatflow_topics::USAGE
            && let Ok(sample) = serde_json::from_value::<UsageSample>(data)
        {
            let mut usage = lock(&self.inner.usage);
            usage.push_back(sample);
            // Older than any window the data set allows is never read again.
            while usage.len() > 100_000 {
                usage.pop_front();
            }
        }
        Box::pin(async {})
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        let inner = self.inner.clone();
        Box::pin(async move {
            // Agents keep working in the chat flow; the successor re-adopts them.
            inner.park.cancel();
            inner.tasks.close();
            let _ = tokio::time::timeout(Duration::from_secs(2), inner.tasks.wait()).await;
        })
    }
}
