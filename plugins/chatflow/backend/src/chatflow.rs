//! The `cc-chatflow` plugin (`chatflow@1`).
//!
//! Runs are background tasks keyed by session. A run is driven only while this instance holds the
//! session's lock (see [`crate::locks`]); on shutdown runs are parked (no terminal status), and the
//! successor resumes every chat session whose last status is `running` from the journal.

use crate::agent_loop::{self, LoopKind, LoopOutcome, LoopSpec, RunEnv, Stops};
use crate::attachments;
use crate::context::{self, Prefix};
use crate::data::{Data, Dataset, Loaded};
use crate::flow;
use crate::host::Host;
use crate::journal::Journal;
use crate::locks;
use crate::projection::project;
use crate::reads::{self, ReadReq, Scope};
use crate::references;
use crate::repetition::{Detector, Verdict};
use crate::schemas;
use cc_protocol::contracts::chatflow::{
    KnowledgeAnswerParams,
    AgentRunParams, AgentRunResult, ApproveParams, Attachment, AttachmentPutParams, AttachmentPutResult, FlowState, PlanCommentParams, PlanEditParams, PlanRefParams, PlanUpdateParams,
    PrewarmParams, PrewarmResult, SessionParams, SteerParams, TurnParams, TurnResult, kinds as chatflow_kinds, plan_status, verbs,
};
use crate::{plan, proposal};
use cc_protocol::contracts::tools::topics as tools_topics;
use cc_protocol::contracts::agents::{Supervise, events as agent_events, topics as agents_topics};
use cc_protocol::contracts::schedule::{Fired, topics as schedule_topics};
use cc_protocol::contracts::frame::{
    JournalEntry, ProjectListResult, Session, SessionDeleted, SessionKind, SessionStatus, SessionUpdateParams, kinds, methods as frame_methods, topics as frame_topics,
};
use cc_protocol::contracts::lifecycle::{Hello, ProbeCheck, ProbeReport};
use cc_protocol::contracts::memory::{Task, methods as memory_methods};
use cc_protocol::contracts::mesh::methods as mesh_methods;
use cc_protocol::contracts::model::{Priority, methods as model_methods};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError, codes, parse_params, to_value};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// This plugin's way of working: its category is `chatflow-coding`, and the models it asks for are
/// tagged `coding` (the same word).
pub const FLOW: &str = cc_protocol::contracts::frame::flows::CODING;

pub fn hello() -> Hello {
    let mut hello = Hello::new("cc-chatflow", env!("CARGO_PKG_VERSION"), &cc_protocol::contracts::frame::flows::category(FLOW), "chatflow@1");
    hello.provides = [
        verbs::TURN,
        verbs::STEER,
        verbs::CANCEL,
        verbs::STATE,
        verbs::RUNNING,
        verbs::AGENT_RUN,
        verbs::APPROVE,
        verbs::ATTACHMENT_PUT,
        verbs::PLAN_UPDATE,
        verbs::PLAN_IMPLEMENT,
        verbs::PLAN_EDIT,
        verbs::PLAN_COMMENT,
        verbs::PLAN_REVISE,
        verbs::KNOWLEDGE_ANSWER,
        verbs::PREWARM,
    ]
        .iter()
        .map(|verb| cc_protocol::contracts::frame::flows::method(FLOW, verb))
        .collect();
    // Background jobs and due routines wake their session; changed skills rebuild the prefixes; a
    // merge of the live folder (the source manager's start-up update) reloads the data set. The
    // plugins a new conversation's block is built from coming up, a project opened and a new
    // project description each have that block prefilled again ([`Chatflow::warm_projects`]).
    hello.subscribes = vec![
        tools_topics::JOB_EVENT.to_owned(),
        agents_topics::SUPERVISE.to_owned(),
        schedule_topics::FIRED.to_owned(),
        SKILLS_CHANGED.to_owned(),
        frame_topics::SESSION_DELETED.to_owned(),
        SOURCE_CHANGED.to_owned(),
        frame_topics::PLUGIN_STATE.to_owned(),
        frame_topics::PROJECT_CHANGED.to_owned(),
        cc_protocol::contracts::memory::topics::KNOWLEDGE_CHANGED.to_owned(),
    ];
    hello
}

/// Published by every plugin that serves skills when its skills changed: the catalog in the
/// prefixes built before it is stale.
const SKILLS_CHANGED: &str = cc_protocol::contracts::skills::topics::CHANGED;

/// Published by the source manager when the live folder changed, among others once its start-up
/// merge is done: this plugin starts beside it and may have read its live data before the merge
/// (seen: the new prompts' data set rejected for a key the merge was about to bring).
const SOURCE_CHANGED: &str = cc_protocol::contracts::source::topics::CHANGED;

type RunOutcome = Option<Result<AgentRunResult, RpcError>>;
type Done = watch::Receiver<RunOutcome>;
type Registered = (Stops, Arc<Mutex<String>>, watch::Sender<RunOutcome>);

struct RunHandle {
    run_id: String,
    stops: Stops,
    stage: Arc<Mutex<String>>,
    done: Done,
}

struct Inner {
    /// Prompts, texts and tunables (reloaded on change).
    data: Arc<Data>,
    runs: Mutex<HashMap<String, RunHandle>>,
    /// Instance shutdown.
    park: CancellationToken,
    tasks: tokio_util::task::TaskTracker,
    opened_projects: Mutex<HashSet<String>>,
    /// Per session: the compacted conversation being prefilled since its turn ended
    /// ([`crate::fit::prepare`]); the session's next turn ends it.
    prewarms: Mutex<HashMap<String, crate::fit::Prewarm>>,
    /// Per session: the state key ([`crate::prewarm::state_key`]) of the prefix last sent to
    /// `model.prewarm`, so a session state is prefilled once however often it is asked for.
    warmed: Mutex<HashMap<String, String>>,
    /// A pass prefilling every project's next-conversation block is running; `warm_again`: asked
    /// for again meanwhile, so it runs once more when done.
    warming: AtomicBool,
    warm_again: AtomicBool,
    /// When a plugin's skills last changed (ms, 0: not since the start): a session's prefix built
    /// before it is rebuilt on its next turn.
    skills_changed_ms: AtomicU64,
    /// Learns every finished run, one at a time at idle priority (started on activation).
    learner: std::sync::OnceLock<crate::learn::Learner>,
    /// Owns the whole replay, including preparation and the gap between its arms.
    replay: Mutex<ReplayState>,
}

#[derive(Default)]
struct ReplayState {
    active: bool,
    cancel: CancellationToken,
    foreground: usize,
}

/// A foreground request owns its preparation until it has registered its run.
struct ForegroundTurn(Arc<Inner>);

impl Drop for ForegroundTurn {
    fn drop(&mut self) {
        let mut replay = self.0.replay.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        replay.foreground -= 1;
    }
}

#[derive(Clone)]
pub struct Chatflow {
    inner: Arc<Inner>,
}

/// Replays' turns through this chat flow: a turn in a replay session, awaited to its end.
struct FlowReplayer {
    flow: Chatflow,
    ctx: Ctx,
}

impl crate::replay::Replayer for FlowReplayer {
    fn idle(&self) -> bool {
        self.flow.lock_runs().values().all(|handle| handle.done.borrow().is_some())
    }

    fn begin(&self) -> Option<crate::replay::ReplayGuard> {
        let mut replay = self.flow.inner.replay.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if replay.active || replay.foreground > 0 || !self.idle() || self.flow.inner.park.is_cancelled() {
            return None;
        }
        replay.active = true;
        replay.cancel = self.flow.inner.park.child_token();
        let cancel = replay.cancel.clone();
        let inner = self.flow.inner.clone();
        Some(crate::replay::ReplayGuard::new(cancel, move || {
            inner.replay.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).active = false;
        }))
    }

    fn turn(&self, session_id: String, text: String, cancel: CancellationToken) -> BoxFuture<'static, Result<String, RpcError>> {
        let flow = self.flow.clone();
        let ctx = self.ctx.clone();
        Box::pin(async move {
            let params = TurnParams { session_id: session_id.clone(), text, mode: "auto".to_owned(), attachments: Vec::new(), spec: false, mentions: Vec::new() };
            let started = flow.turn_inner(ctx.clone(), params, Some(cancel)).await?;
            if let Some((.., mut done)) = flow.active(&session_id) {
                while done.borrow().is_none() {
                    if done.changed().await.is_err() {
                        break;
                    }
                }
            }
            let entries = Host::new(ctx.clone()).journal_read(&session_id, 0).await?;
            let state = entries
                .iter()
                .rev()
                .find(|entry| entry.kind == kinds::STATUS && str_field(&entry.data, "run_id") == started.run_id && str_field(&entry.data, "state") != "running")
                .map(|entry| str_field(&entry.data, "state").to_owned())
                .unwrap_or_else(|| "cancelled".to_owned());
            Ok(state)
        })
    }
}

impl Chatflow {
    /// A chat flow starting from the shipped data set.
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            inner: Arc::new(Inner {
                data: Arc::new(Data::new()?),
                runs: Mutex::new(HashMap::new()),
                park: CancellationToken::new(),
                tasks: tokio_util::task::TaskTracker::new(),
                opened_projects: Mutex::new(HashSet::new()),
                prewarms: Mutex::new(HashMap::new()),
                warmed: Mutex::new(HashMap::new()),
                warming: AtomicBool::new(false),
                warm_again: AtomicBool::new(false),
                skills_changed_ms: AtomicU64::new(0),
                learner: std::sync::OnceLock::new(),
                replay: Mutex::new(ReplayState::default()),
            }),
        })
    }

    /// The directory layers of the data set: `data_dir` from the plugin config (shipped files),
    /// then the live data in the plugin's folder of the live folder (taken once it exists).
    fn data_layers(ctx: &Ctx) -> Vec<(String, PathBuf)> {
        let mut layers = Vec::new();
        if let Some(dir) = ctx.config().get("data_dir").and_then(Value::as_str) {
            let dir = match dir.strip_prefix("~/") {
                Some(rest) => std::env::var("HOME").map(|home| PathBuf::from(home).join(rest)).unwrap_or_else(|_| PathBuf::from(dir)),
                None => PathBuf::from(dir),
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
            match loaded {
                Loaded::Rejected(error) => tracing::warn!(%error, "data set rejected; the previous one stays active"),
                _ => tracing::info!(version = %self.inner.data.get().version(), "data set active"),
            }
            ctx.publish(DATA_TOPIC, event);
        }
    }

    /// Load the data layers now and watch them for changes.
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

/// Event topic of data set loads: `{ok, version, sources}` or `{ok: false, error, version}`.
pub const DATA_TOPIC: &str = "chatflow.data";

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

/// What a background job's event tells the model: how it exited (or the line that matched, or how
/// long it was silent), its output's tail, and where its whole output is.
fn job_event_text(set: &crate::data::Dataset, data: &Value) -> String {
    let job_id = str_field(data, "job_id");
    let output = str_field(data, "output_tail").trim_end();
    let running = str_field(data, "state") == "running";
    let mut text = match (str_field(data, "reason"), str_field(data, "state")) {
        ("match", _) => set.text("job_matched", &[("job_id", job_id), ("matched", str_field(data, "matched").trim_end())]),
        ("silence", _) => {
            let seconds = data.get("silent_s").and_then(Value::as_f64).map(|s| format!("{}", s.round() as u64)).unwrap_or_default();
            set.text("job_silent", &[("job_id", job_id), ("seconds", &seconds), ("output", output)])
        }
        (_, "killed") => set.text("job_killed", &[("job_id", job_id), ("output", output)]),
        (_, _) => match data.get("exit_code").and_then(Value::as_i64) {
            Some(code) => set.text("job_exited", &[("job_id", job_id), ("exit", &code.to_string()), ("output", output)]),
            None => set.text("job_lost", &[("job_id", job_id), ("output", output)]),
        },
    };
    let artifact = str_field(data, "artifact");
    if !artifact.is_empty() {
        let key = if running { "job_artifact_running" } else { "job_artifact" };
        text.push_str(&set.text(key, &[("artifact", artifact)]));
    }
    text
}

/// A turn a card starts with its route decided: the message text, extra fields of its
/// `user_message` and `route` entries, and the stage the status starts at.
struct CardTurn {
    route: &'static str,
    stage: &'static str,
    text: String,
    message: Value,
    route_fields: Value,
}

/// `extra`'s fields set on `target` (both objects).
fn merge_fields(target: &mut Value, extra: Value) {
    if let (Some(target), Value::Object(extra)) = (target.as_object_mut(), extra) {
        target.extend(extra);
    }
}

fn new_run_id() -> String {
    format!("run-{}", uuid::Uuid::new_v4().simple())
}

fn data_dir(ctx: &Ctx) -> PathBuf {
    ctx.env.data_dir.clone()
}

/// Whether the last status of a journal says a run is in progress.
/// The text of the newest supervision note no MANAGE decided yet (written after the last
/// `managed` note), if any.
fn unanswered_supervision(journal: &Journal) -> Option<String> {
    let decided = journal
        .entries()
        .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "managed")
        .map(|entry| entry.seq)
        .max()
        .unwrap_or(0);
    journal
        .entries()
        .filter(|entry| entry.seq > decided && entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "supervise")
        .last()
        .map(|entry| str_field(&entry.data, "text").to_owned())
}

fn running_status(journal: &Journal) -> Option<JournalEntry> {
    journal.last_status().filter(|entry| str_field(&entry.data, "state") == "running").cloned()
}

impl Chatflow {
    fn lock_runs(&self) -> std::sync::MutexGuard<'_, HashMap<String, RunHandle>> {
        match self.inner.runs.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Register a run for `session_id` unless one is active. Returns the stops and the sender.
    fn register(&self, session_id: &str, run_id: &str) -> Option<Registered> {
        // Non-replay entrances (plans, agent runs and resumed runs) also yield background work.
        let replay = self.inner.replay.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        replay.cancel.cancel();
        self.register_with_cancel(session_id, run_id, None)
    }

    fn register_with_cancel(&self, session_id: &str, run_id: &str, cancel: Option<CancellationToken>) -> Option<Registered> {
        let mut runs = self.lock_runs();
        if runs.get(session_id).is_some_and(|handle| handle.done.borrow().is_none()) {
            return None;
        }
        let stops = Stops { cancel: cancel.map(|token| token.child_token()).unwrap_or_default(), park: self.inner.park.child_token() };
        let stage = Arc::new(Mutex::new("starting".to_owned()));
        let (sender, done) = watch::channel(None);
        runs.insert(session_id.to_owned(), RunHandle { run_id: run_id.to_owned(), stops: stops.clone(), stage: stage.clone(), done });
        Some((stops, stage, sender))
    }

    /// Keep `prewarm` as the session's (None ends the one kept; dropping a prewarm cancels it).
    /// Queue a finished run for learning (nothing before activation started the learner).
    fn learn(&self, session_id: &str, run_id: &str) {
        if let Some(learner) = self.inner.learner.get() {
            learner.learn(session_id, run_id);
        }
    }

    fn set_prewarm(&self, session_id: &str, prewarm: Option<crate::fit::Prewarm>) {
        if let Ok(mut prewarms) = self.inner.prewarms.lock() {
            match prewarm {
                Some(prewarm) => prewarms.insert(session_id.to_owned(), prewarm),
                None => prewarms.remove(session_id),
            };
        }
    }

    /// Whether a prefill of the session's prefix is outstanding.
    fn prewarming(&self, session_id: &str) -> bool {
        self.inner.prewarms.lock().map(|prewarms| prewarms.contains_key(session_id)).unwrap_or(false)
    }

    fn active(&self, session_id: &str) -> Option<(String, Stops, Arc<Mutex<String>>, Done)> {
        let runs = self.lock_runs();
        runs.get(session_id)
            .filter(|handle| handle.done.borrow().is_none())
            .map(|handle| (handle.run_id.clone(), handle.stops.clone(), handle.stage.clone(), handle.done.clone()))
    }

    /// `chatflow.running`: the sessions with a run going now.
    fn running(&self) -> cc_protocol::contracts::chatflow::RunningResult {
        let runs = self.lock_runs();
        let mut sessions: Vec<String> = runs.iter().filter(|(_, handle)| handle.done.borrow().is_none()).map(|(session, _)| session.clone()).collect();
        sessions.sort();
        cc_protocol::contracts::chatflow::RunningResult { sessions }
    }

    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.inner.tasks.spawn(future);
    }

    async fn ensure_project_open(&self, host: &Host, project_id: &str, root: &str) {
        let first = self.inner.opened_projects.lock().map(|mut opened| opened.insert(project_id.to_owned())).unwrap_or(true);
        if first {
            context::open_project(host, project_id, root).await;
        }
    }

    /// Everything a run needs about its session.
    #[allow(clippy::too_many_arguments)]
    async fn environment(
        &self,
        host: &Host,
        journal: &mut Journal,
        session: &Session,
        run_id: &str,
        mode: &str,
        priority: Priority,
        stops: Stops,
        stage: Arc<Mutex<String>>,
    ) -> Result<RunEnv, RpcError> {
        if stops.cancel.is_cancelled() || stops.park.is_cancelled() {
            return Err(RpcError::cancelled());
        }
        // The run's first wait is opening the project's code graph: say so.
        if let Ok(mut current) = stage.lock() {
            *current = "context".into();
        }
        let project = host.project_get(&session.project_id).await?;
        self.ensure_project_open(host, &project.id, &project.root).await;
        if stops.cancel.is_cancelled() || stops.park.is_cancelled() {
            return Err(RpcError::cancelled());
        }
        let data = self.inner.data.get();
        // A background agent's system prompt carries its task loop's rules only.
        let role = if matches!(priority, Priority::Agent) { crate::data::ROLE_AGENT } else { crate::data::ROLE_CHAT };
        let skills_changed_ms = self.inner.skills_changed_ms.load(Ordering::SeqCst);
        let prefix: Prefix = context::ensure(journal, &project.id, &data, role, skills_changed_ms).await?;
        let workspace = if session.workspace.is_empty() { project.root.clone() } else { session.workspace.clone() };
        let info = context::model_info(host).await;
        // A session runs on one model: its first run records it, and a run on another model (after
        // an engine or variant switch) is refused, so a conversation never continues on a model
        // that did not write it.
        if let Some(info) = &info {
            match &session.model {
                None => {
                    let record = SessionUpdateParams { id: session.id.clone(), title: None, status: None, task_id: None, model: Some(info.model_id.clone()) };
                    host.session_update(&record).await?;
                }
                Some(model) if model != &info.model_id => {
                    return Err(RpcError::new(
                        codes::CONFLICT,
                        format!(
                            "This session ran on {model}. The model is now {}, and a session is not continued on another model: start a new session, or switch the engine back in Settings > Model.",
                            info.model_id
                        ),
                    ));
                }
                Some(_) => {}
            }
        }
        // Attached images are shown to the model only when it says it takes them.
        let image_input = info.is_some_and(|info| info.accepts_images());
        Ok(RunEnv {
            host: host.clone(),
            session_id: session.id.clone(),
            scope: Scope { project_id: project.id.clone(), workspace, root: project.root.clone() },
            prefix,
            run_id: run_id.to_owned(),
            mode: mode.to_owned(),
            priority,
            stops,
            data,
            stage,
            image_input,
        })
    }

    // ---- chatflow.prewarm ---------------------------------------------------------------------

    /// Prefill every project's next-conversation block in the background, so a new conversation's
    /// first call reads only its message whether or not the chat window asked in time (at start
    /// the window asks before the model is up, and that ask fails). Passes asked for while one
    /// runs fold into one more; a project whose block cannot be built yet (the model or memory not
    /// up) is skipped until the next pass.
    fn warm_projects(&self, ctx: Ctx) {
        self.inner.warm_again.store(true, Ordering::SeqCst);
        if self.inner.warming.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        self.spawn(async move {
            while this.inner.warm_again.swap(false, Ordering::SeqCst) && !this.inner.park.is_cancelled() {
                let projects = match Host::new(ctx.clone()).get::<ProjectListResult>(frame_methods::PROJECT_LIST, json!({})).await {
                    Ok(listed) => listed.projects,
                    Err(error) => {
                        tracing::debug!(%error, "no projects to prefill a new conversation for");
                        Vec::new()
                    }
                };
                for project in projects.into_iter().filter(|project| project.flows.iter().any(|flow| flow == FLOW)) {
                    match this.prewarm_project(ctx.clone(), &project.id).await {
                        Ok(result) => tracing::debug!(project = %project.id, state = %result.state, "a new conversation's block"),
                        Err(error) => tracing::debug!(project = %project.id, %error, "a new conversation's block is not prefilled yet"),
                    }
                }
            }
            this.inner.warming.store(false, Ordering::SeqCst);
            // Asked for between the last check and the flag going down: run that pass too.
            if this.inner.warm_again.load(Ordering::SeqCst) {
                this.warm_projects(ctx);
            }
        });
    }

    /// Prefill the session's prompt prefix (system message and conversation) in the background, so
    /// its next turn's first call reads only the new message ([`crate::prewarm`]); without a
    /// session, the project's next conversation's system block.
    async fn prewarm(&self, ctx: Ctx, params: PrewarmParams) -> Result<PrewarmResult, RpcError> {
        match (params.session_id, params.project_id) {
            (Some(session_id), _) => self.prewarm_session(ctx, SessionParams { session_id }).await,
            (None, Some(project_id)) => self.prewarm_project(ctx, &project_id).await,
            (None, None) => Err(RpcError::invalid_params("session_id or project_id is required")),
        }
    }

    /// Prefill the chat system block a new conversation of the project starts with, while it is
    /// written (it is created when sent). The model keeps the block for whichever conversation
    /// starts with it; nothing is kept under the cache key used here.
    async fn prewarm_project(&self, ctx: Ctx, project_id: &str) -> Result<PrewarmResult, RpcError> {
        let host = Host::new(ctx);
        let project = host.project_get(project_id).await?;
        self.ensure_project_open(&host, &project.id, &project.root).await;
        let data = self.inner.data.get();
        let prefix = context::build(&host, &project.id, &data, crate::data::ROLE_CHAT).await?;
        if prefix.ctx_window == 0 {
            return Ok(PrewarmResult { state: "skipped".into(), messages: 0 });
        }
        let messages = crate::prewarm::next_conversation(&prefix, &data);
        let count = messages.len() as u32;
        let cache_key = format!("{}:next", project.id);
        let key = crate::prewarm::state_key(&cache_key, &messages, &prefix.tools);
        {
            let Ok(mut warmed) = self.inner.warmed.lock() else { return Ok(PrewarmResult { state: "skipped".into(), messages: 0 }) };
            if warmed.get(&cache_key) == Some(&key) {
                return Ok(PrewarmResult { state: "warm".into(), messages: count });
            }
            warmed.insert(cache_key.clone(), key.clone());
        }
        let this = self.clone();
        let owner = cache_key.clone();
        let handle = crate::prewarm::send(&host, &cache_key, messages, &prefix.tools, "the next conversation's system block", true, crate::prewarm::When::Soon, move |_| {
            // Deduplicated only while in flight: the model may drop the block later (least recently
            // used), and asking for a block it keeps is answered at once, without a slot.
            if let Ok(mut warmed) = this.inner.warmed.lock() && warmed.get(&owner) == Some(&key) {
                warmed.remove(&owner);
            }
        });
        // A newer block for the project (the repository map changed) replaces this one.
        self.set_prewarm(&cache_key, Some(handle));
        Ok(PrewarmResult { state: "started".into(), messages: count })
    }

    async fn prewarm_session(&self, ctx: Ctx, params: SessionParams) -> Result<PrewarmResult, RpcError> {
        let state = |state: &str| PrewarmResult { state: state.to_owned(), messages: 0 };
        if self.active(&params.session_id).is_some() {
            return Ok(state("running"));
        }
        let host = Host::new(ctx);
        let session = host.session_get(&params.session_id).await?;
        if session.kind != SessionKind::Chat {
            return Ok(state("skipped"));
        }
        let mut journal = Journal::load(host.clone(), &params.session_id).await?;
        if running_status(&journal).is_some() {
            return Ok(state("running"));
        }
        let project = host.project_get(&session.project_id).await?;
        self.ensure_project_open(&host, &project.id, &project.root).await;
        let data = self.inner.data.get();
        let skills_changed_ms = self.inner.skills_changed_ms.load(Ordering::SeqCst);
        let prefix = context::ensure(&mut journal, &project.id, &data, crate::data::ROLE_CHAT, skills_changed_ms).await?;
        let image_input = context::model_info(&host).await.is_some_and(|info| info.accepts_images());
        // A turn may have started while the prefix was read: its own calls keep the cache.
        if self.active(&params.session_id).is_some() {
            return Ok(state("running"));
        }
        // The user opened the conversation: they are about to write in it.
        Ok(self.warm(&host, &params.session_id, &prefix, &data, image_input, &journal.entries_vec(), crate::prewarm::When::Soon))
    }

    /// Send the prefill of the session's prefix unless this exact state was sent already.
    #[allow(clippy::too_many_arguments)]
    fn warm(&self, host: &Host, session_id: &str, prefix: &Prefix, data: &Dataset, image_input: bool, entries: &[JournalEntry], when: crate::prewarm::When) -> PrewarmResult {
        if prefix.ctx_window == 0 {
            // No model size known (model.info failed): the model is not there to prefill.
            return PrewarmResult { state: "skipped".into(), messages: 0 };
        }
        let messages = crate::prewarm::closed(crate::prewarm::conversation(prefix, data, entries, image_input));
        let count = messages.len() as u32;
        // An idle-only prefill may have stopped early for other work: the same state asked for
        // soon (the user opened the conversation) is sent again.
        let mut key = crate::prewarm::state_key(session_id, &messages, &prefix.tools);
        if when == crate::prewarm::When::Idle {
            key.push_str("/idle");
        }
        {
            let Ok(mut warmed) = self.inner.warmed.lock() else { return PrewarmResult { state: "skipped".into(), messages: 0 } };
            if warmed.get(session_id) == Some(&key) {
                return PrewarmResult { state: "warm".into(), messages: count };
            }
            warmed.insert(session_id.to_owned(), key.clone());
        }
        let this = self.clone();
        let session = session_id.to_owned();
        let handle = crate::prewarm::send(host, session_id, messages, &prefix.tools, "the conversation", false, when, move |ok| {
            // A refused or failed prefill may be asked for again.
            if !ok && let Ok(mut warmed) = this.inner.warmed.lock() && warmed.get(&session) == Some(&key) {
                warmed.remove(&session);
            }
        });
        self.set_prewarm(session_id, Some(handle));
        PrewarmResult { state: "started".into(), messages: count }
    }

    // ---- chatflow.turn ------------------------------------------------------------------------

    async fn turn(&self, ctx: Ctx, params: TurnParams) -> Result<TurnResult, RpcError> {
        self.turn_inner(ctx, params, None).await
    }

    async fn turn_inner(&self, ctx: Ctx, params: TurnParams, replay_cancel: Option<CancellationToken>) -> Result<TurnResult, RpcError> {
        let host = Host::new(ctx.clone());
        let replay = replay_cancel.is_some();
        let _foreground = if replay {
            None
        } else {
            let mut state = self.inner.replay.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.foreground += 1;
            state.cancel.cancel();
            Some(ForegroundTurn(self.inner.clone()))
        };
        if replay_cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return Err(RpcError::new(codes::CANCELLED, "replay cancelled"));
        }
        let attached = self.prepare_attachments(&host, params.attachments.clone()).await?;
        // Other repositories the message references: opened read-only, named to the run in a note.
        let referenced = references::resolve(&host, &self.inner.data.get(), &params.session_id, &params.mentions, &attached).await?;
        let note = |run_id: &str| referenced.clone().map(|mut note| {
            note["run_id"] = json!(run_id);
            note
        });
        let steer = references::with(attachments::with(json!({"text": params.text, "kind": "steer", "target": "chat", "consumed": false}), &attached), &params.mentions);
        if let Some((run_id, ..)) = self.active(&params.session_id) {
            host.journal_append(&params.session_id, kinds::STEER, steer).await?;
            if let Some(note) = note(&run_id) {
                host.journal_append(&params.session_id, kinds::NOTE, note).await?;
            }
            return Ok(TurnResult { run_id, queued_as_steer: true });
        }
        let mut journal = Journal::load(host.clone(), &params.session_id).await?;
        if let Some(status) = running_status(&journal) {
            // Driven by another instance (a swap in progress) or about to be resumed: steer it.
            let run_id = str_field(&status.data, "run_id").to_owned();
            journal.append(kinds::STEER, steer).await?;
            if let Some(note) = note(&run_id) {
                journal.append(kinds::NOTE, note).await?;
            }
            return Ok(TurnResult { run_id, queued_as_steer: true });
        }
        let run_id = new_run_id();
        if replay_cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return Err(RpcError::new(codes::CANCELLED, "replay cancelled"));
        }
        let Some((stops, stage, sender)) = self.register_with_cancel(&params.session_id, &run_id, replay_cancel) else {
            journal.append(kinds::STEER, steer).await?;
            let run_id = self.active(&params.session_id).map(|active| active.0).unwrap_or_default();
            if let Some(note) = note(&run_id) {
                journal.append(kinds::NOTE, note).await?;
            }
            return Ok(TurnResult { run_id, queued_as_steer: true });
        };
        let mut message = json!({"text": params.text, "run_id": run_id});
        if !params.mentions.is_empty() {
            message["mentions"] = json!(params.mentions);
        }
        let first_message = !journal.entries().any(|entry| entry.kind == kinds::USER_MESSAGE);
        journal.append(kinds::USER_MESSAGE, attachments::with(message, &attached)).await?;
        // The conversation's name comes from its first message; without one it would stay "New
        // conversation" among twenty others.
        if first_message
            && let Ok(session) = host.session_get(&params.session_id).await
            && session.title.trim().is_empty()
        {
            let title = title_from(&params.text);
            if !title.is_empty() {
                let record = SessionUpdateParams { id: params.session_id.clone(), title: Some(title), status: None, task_id: None, model: None };
                if let Err(error) = host.session_update(&record).await {
                    tracing::debug!(error = %error.message, "the conversation was not named");
                }
            }
        }
        if let Some(note) = note(&run_id) {
            journal.append(kinds::NOTE, note).await?;
        }
        let status_seq = journal
            .append(kinds::STATUS, json!({"state": "running", "run_id": run_id, "stage": "route", "mode": params.mode, "kind": "turn", "data_version": self.inner.data.get().version()}))
            .await?;
        let _ = host.session_status(&params.session_id, SessionStatus::Running).await;
        // The run's starting point, for replaying a lesson learned from it later (`replay.rs`).
        if !replay {
            let snapshot_host = host.clone();
            let paths = crate::replay::Paths::new(&data_dir(&ctx));
            let (session_id, run) = (params.session_id.clone(), run_id.clone());
            tokio::spawn(async move {
                let Ok(session) = snapshot_host.session_get(&session_id).await else { return };
                if let Err(reason) = crate::replay::snapshot(&snapshot_host, &paths, &session_id, &run, &session.workspace).await {
                    tracing::debug!(%reason, session = %session_id, "no snapshot of the run's start");
                }
            });
        }
        let this = self.clone();
        let session_id = params.session_id.clone();
        let mode = params.mode.clone();
        let run = run_id.clone();
        self.spawn(async move {
            let outcome = this.drive_foreground(ctx, &session_id, run, status_seq, mode, stops, stage).await;
            let _ = sender.send(Some(outcome.map(|_| empty_result("done"))));
        });
        Ok(TurnResult { run_id, queued_as_steer: false })
    }

    /// Drive foreground runs of a session until no follow-up is pending.
    #[allow(clippy::too_many_arguments)]
    async fn drive_foreground(
        &self,
        ctx: Ctx,
        session_id: &str,
        mut run_id: String,
        mut status_seq: u64,
        mut mode: String,
        stops: Stops,
        stage: Arc<Mutex<String>>,
    ) -> Result<(), RpcError> {
        let host = Host::new(ctx.clone());
        let Some(_lock) = locks::lock(&data_dir(&ctx), session_id, &stops.park).await.map_err(|error| RpcError::internal(error.to_string()))? else {
            return Ok(());
        };
        // The conversation stays resident while its turns run: waiting on a long tool (a build, a
        // test suite) is not idle, and spilling it would write gigabytes to disk mid-turn. Released
        // when the turns end; the model plugin also lets a hold lapse by itself.
        let _ = host.call(model_methods::HOLD, json!({"cache_key": session_id, "hold": true})).await;
        let result: Result<(), RpcError> = async {
        loop {
            // A prewarm since the last turn ended stops here: the turn continues from what it did.
            self.set_prewarm(session_id, None);
            let mut journal = Journal::load(host.clone(), session_id).await?;
            let mut ran: Option<RunEnv> = None;
            let outcome = async {
                let session = host.session_get(session_id).await?;
                let env = self
                    .environment(&host, &mut journal, &session, &run_id, &mode, Priority::Foreground, stops.clone(), stage.clone())
                    .await?;
                ran = Some(env.clone());
                // Where the turn's model calls run is decided per call (`llm::chat` claims the best
                // free model in the network before each one); the run's end gives its place back.
                let wake = journal.get(status_seq).is_some_and(|entry| str_field(&entry.data, "kind") == "wake");
                if !wake {
                    return flow::run_turn(&env, &mut journal, status_seq).await;
                }
                // Woken by a background job: continue the work in the agent loop.
                let spec = LoopSpec { kind: LoopKind::Foreground, effort: env.data.effort.default.clone(), task: None, step: None };
                let outcome = agent_loop::run(&env, &mut journal, &spec).await?;
                Ok(flow::TurnEnd { state: outcome.outcome.clone(), detail: outcome.summary.clone(), route: "DO".into(), agent: Some(outcome) })
            }
            .await;
            let (state, detail) = match &outcome {
                Ok(end) => (end.state.clone(), end.detail.clone()),
                Err(error) if error.code == codes::CANCELLED && stops.park.is_cancelled() => ("parked".into(), String::new()),
                Err(error) if error.code == codes::CANCELLED => ("cancelled".into(), "Cancelled.".into()),
                Err(error) => {
                    let _ = journal.append(kinds::ERROR, json!({"message": error.message, "code": error.code, "run_id": run_id})).await;
                    ("failed".into(), error.message.clone())
                }
            };
            if state == "parked" {
                return Ok(());
            }
            journal.sync().await?;
            // The host, not the model, clears what the turn left behind that is nobody's work
            // (the scratch folder, files its commands made); the change itself stays. Before the
            // terminal status, so whoever waits for it sees the workspace as the turn leaves it.
            if let Some(env) = &ran {
                let since_ms = journal.get(status_seq).map(|entry| entry.ts_ms).unwrap_or(0);
                let params = json!({"workspace": env.scope.workspace, "session_id": session_id, "since_ms": since_ms});
                match host.call(cc_protocol::contracts::tools::methods::CLEANUP, params).await {
                    Ok(result) => {
                        let removed = result.get("removed").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                        if removed > 0 {
                            tracing::info!(session = %session_id, removed, "cleared what the turn left behind");
                        }
                    }
                    Err(error) => tracing::debug!(%error, "turn cleanup failed"),
                }
                // What the turn changed, from the step log (edits, commands' effects, deletions),
                // kept in the transcript as the turn's Changed card even after a commit empties the log.
                if let Some(note) = crate::afteraction::changed_note(&host, &env.scope.workspace, session_id, since_ms, &run_id).await {
                    journal.append(kinds::NOTE, note).await?;
                }
            }
            let _ = host.call(mesh_methods::PLACE_END, json!({"session_id": session_id})).await;
            journal.append(kinds::STATUS, json!({"state": state, "detail": detail, "run_id": run_id, "data_version": self.inner.data.get().version()})).await?;
            self.learn(session_id, &run_id);
            let session_status = match state.as_str() {
                "done" => SessionStatus::Done,
                "cancelled" => SessionStatus::Cancelled,
                "blocked" | "stopped" => SessionStatus::Waiting,
                _ => SessionStatus::Failed,
            };
            if state == "cancelled" {
                let _ = host.session_status(session_id, session_status).await;
                return Ok(());
            }
            // The user reads the answer now: compact a long conversation and let the model re-read
            // it meanwhile, so the next turn starts warm.
            if let Some(env) = &ran {
                let conversation = |entries: &[JournalEntry]| crate::prewarm::conversation(&env.prefix, &env.data, entries, env.image_input);
                let prewarm = crate::fit::prepare(env, &mut journal, u64::MAX, env.data.limits.compact_idle_tokens, crate::prewarm::When::Idle, conversation).await;
                self.set_prewarm(session_id, prewarm);
            }
            // Messages that arrived too late for this run, then queued follow-ups, each start a turn.
            let next = journal.pending_steers("steer").into_iter().chain(journal.pending_steers("followup")).next();
            // Delegated work that needed a decision while this turn ran (an agent blocked, a need
            // for the lead): the session decides it now, before it rests.
            // Only after a turn that ended well: a MANAGE that failed would otherwise start again.
            if next.is_none()
                && state == "done"
                && let Some(work) = unanswered_supervision(&journal)
            {
                run_id = new_run_id();
                if let Ok(mut current) = stage.lock() {
                    *current = "manage".into();
                }
                status_seq = journal
                    .append(kinds::STATUS, json!({"state": "running", "run_id": run_id, "stage": "manage", "mode": mode, "kind": "turn", "data_version": self.inner.data.get().version()}))
                    .await?;
                let detail = json!({"work": work, "route": "MANAGE", "effort": self.inner.data.get().effort.default, "by": "host"});
                journal
                    .append(kinds::ROUTE, json!({"route": "MANAGE", "stage": "route", "detail": detail, "raw": "", "run_id": run_id}))
                    .await?;
                if let Ok(mut runs) = self.inner.runs.lock()
                    && let Some(handle) = runs.get_mut(session_id)
                {
                    handle.run_id = run_id.clone();
                }
                continue;
            }
            let Some(next) = next else {
                let _ = host.session_status(session_id, session_status).await;
                // The next turn's prefix is known now: prefill it while the user reads the answer
                // (a compaction above prefilled its own already). Whether a next turn comes is a
                // guess, so only while the model has nothing else to do.
                if let Some(env) = &ran
                    && !self.prewarming(session_id)
                {
                    let _ = journal.sync().await;
                    self.warm(&host, session_id, &env.prefix, &env.data, env.image_input, &journal.entries_vec(), crate::prewarm::When::Idle);
                }
                return Ok(());
            };
            journal.set(next.seq, "as_turn", json!(true)).await?;
            journal.consume_steer(next.seq).await?;
            run_id = new_run_id();
            if let Ok(mut current) = stage.lock() {
                *current = "route".into();
            }
            journal
                .append(kinds::USER_MESSAGE, references::carry(attachments::carry(json!({"text": str_field(&next.data, "text"), "run_id": run_id, "from_steer": next.seq}), &next.data), &next.data))
                .await?;
            status_seq = journal
                .append(kinds::STATUS, json!({"state": "running", "run_id": run_id, "stage": "route", "mode": mode, "kind": "turn", "data_version": self.inner.data.get().version()}))
                .await?;
            mode = mode.clone();
            if let Ok(mut runs) = self.inner.runs.lock()
                && let Some(handle) = runs.get_mut(session_id)
            {
                handle.run_id = run_id.clone();
            }
        }
        }
        .await;
        let _ = host.call(model_methods::HOLD, json!({"cache_key": session_id, "hold": false})).await;
        result
    }

    // ---- knowledge, plans -----------------------------------------------------------------------

    /// Accept (with the text as edited) or reject a knowledge item the model proposed; an accepted
    /// one is written to the project's knowledge, bound to the code it named, with the journal entry
    /// as its evidence (docs/PROJECT-KNOWLEDGE-PLAN.md P3).
    async fn knowledge_answer(&self, ctx: Ctx, params: KnowledgeAnswerParams) -> Result<Value, RpcError> {
        let host = Host::new(ctx);
        let entry = host
            .journal_read(&params.session_id, params.seq)
            .await?
            .into_iter()
            .find(|entry| entry.seq == params.seq)
            .ok_or_else(|| RpcError::not_found("no such journal entry"))?;
        if entry.kind != chatflow_kinds::KNOWLEDGE {
            return Err(RpcError::invalid_params("that entry is not a knowledge item"));
        }
        let current = str_field(&entry.data, "state").to_owned();
        if current != proposal::PROPOSED {
            return Ok(json!({"state": current, "changed": false}));
        }
        let state = if params.accept { proposal::ACCEPTED } else { proposal::REJECTED };
        host.journal_set(&params.session_id, params.seq, "state", json!(state)).await?;
        if !params.accept {
            return Ok(json!({"state": state, "changed": true}));
        }
        let proposed = str_field(&entry.data, "text").trim().to_owned();
        let text = params.text.as_deref().map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned).unwrap_or_else(|| proposed.clone());
        if text != proposed {
            host.journal_set(&params.session_id, params.seq, "text", json!(text)).await?;
        }
        let kind = str_field(&entry.data, "kind").to_owned();
        let rel = if kind == "concept" { "implemented_by" } else { "about" };
        let binds: Vec<Value> = entry
            .data
            .get("binds")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).map(|name| json!({"rel": rel, "target": name})).collect())
            .unwrap_or_default();
        let item = json!({
            "kind": kind, "name": str_field(&entry.data, "name"), "text": text, "state": str_field(&entry.data, "item_state"), "source": "user", "by": "user",
            "evidence": [format!("journal://{}/{}", params.session_id, params.seq)], "links": binds
        });
        let stored = self.remember(&host, &params.session_id, params.seq, item).await;
        Ok(json!({"state": state, "changed": true, "stored": stored}))
    }

    /// Write one item to the session project's knowledge and keep its id on the journal entry.
    /// False (and a warning) when memory refused or is away: the answer stands, the item does not.
    async fn remember(&self, host: &Host, session_id: &str, seq: u64, mut item: Value) -> bool {
        let project_id = match host.session_get(session_id).await {
            Ok(session) => session.project_id,
            Err(error) => {
                tracing::warn!(%error, session_id, "knowledge not stored: the session is unknown");
                return false;
            }
        };
        item["project_id"] = json!(project_id);
        match host.call(memory_methods::KNOWLEDGE_UPSERT, item).await {
            Ok(stored) => {
                if let Some(id) = stored.get("id").and_then(Value::as_str) {
                    let _ = host.journal_set(session_id, seq, "knowledge_id", json!(id)).await;
                }
                true
            }
            Err(error) => {
                tracing::warn!(%error, session_id, seq, "knowledge not stored");
                false
            }
        }
    }

    /// Approve a draft plan, or return an approved one to draft.
    async fn plan_update(&self, ctx: Ctx, params: PlanUpdateParams) -> Result<Value, RpcError> {
        let host = Host::new(ctx);
        let journal = Journal::load(host.clone(), &params.session_id).await?;
        let Some((seq, current)) = plan::version_of(&journal.entries_vec(), &params.plan_id) else { return Err(RpcError::not_found("no such plan")) };
        let allowed = [plan_status::DRAFT, plan_status::APPROVED];
        if !allowed.contains(&params.status.as_str()) {
            return Err(RpcError::invalid_params("a plan can be set to draft or approved"));
        }
        if !current.is_open() {
            return Err(RpcError::invalid_params(format!("the plan is {}", current.status)));
        }
        if current.status == params.status {
            return Ok(json!({"status": current.status, "changed": false}));
        }
        host.journal_set(&params.session_id, seq, "status", json!(params.status)).await?;
        Ok(json!({"status": params.status, "changed": true}))
    }

    /// The plan card's Implement: a turn whose route is IMPLEMENT (no router call), the plan named.
    async fn plan_implement(&self, ctx: Ctx, params: PlanRefParams) -> Result<TurnResult, RpcError> {
        let host = Host::new(ctx.clone());
        let busy = "a run is going in this conversation; implement the plan when it ends";
        let journal = self.idle_journal(&host, &params.session_id, busy).await?;
        let Some((_, current)) = plan::version_of(&journal.entries_vec(), &params.plan_id) else { return Err(RpcError::not_found("no such plan")) };
        if !current.is_open() {
            return Err(RpcError::invalid_params(format!("the plan is {}", current.status)));
        }
        let text = self.inner.data.get().text("implement_request", &[]);
        let card = CardTurn { route: "IMPLEMENT", stage: "implement", text, message: json!({"implement": params.plan_id}), route_fields: json!({"plan_id": params.plan_id}) };
        self.start_card_turn(ctx, journal, &params.session_id, &params.mode, card, busy).await
    }

    /// The plan card's Revise: the plan's open comments as one change request, each under the part
    /// it is about, in a turn whose route is PLAN (no router call), so PLAN_EDIT writes the next
    /// version. The comments are marked sent with the run.
    async fn plan_revise(&self, ctx: Ctx, params: PlanRefParams) -> Result<TurnResult, RpcError> {
        let host = Host::new(ctx.clone());
        let busy = "a run is going in this conversation; send the comments when it ends";
        let journal = self.idle_journal(&host, &params.session_id, busy).await?;
        let entries = journal.entries_vec();
        let Some((_, current)) = plan::version_of(&entries, &params.plan_id) else { return Err(RpcError::not_found("no such plan")) };
        if !current.is_open() {
            return Err(RpcError::invalid_params(format!("the plan is {}", current.status)));
        }
        let comments = plan::open_comments(&entries, &params.plan_id);
        if comments.is_empty() {
            return Err(RpcError::invalid_params("the plan has no open comments"));
        }
        let text = plan::revise_request(&self.inner.data.get(), &current, &comments);
        let card = CardTurn { route: "PLAN", stage: "plan", text, message: json!({"revise": params.plan_id}), route_fields: json!({"plan_id": params.plan_id}) };
        let result = self.start_card_turn(ctx, journal, &params.session_id, &params.mode, card, busy).await?;
        for comment in comments {
            host.journal_set(&params.session_id, comment.seq, "state", json!("sent")).await?;
            host.journal_set(&params.session_id, comment.seq, "run_id", json!(result.run_id)).await?;
        }
        Ok(result)
    }

    /// The session's journal when no run is going in it (else `busy` with `message`).
    async fn idle_journal(&self, host: &Host, session_id: &str, message: &str) -> Result<Journal, RpcError> {
        let busy = || RpcError::new(codes::BUSY, message.to_owned());
        if self.active(session_id).is_some() {
            return Err(busy());
        }
        let journal = Journal::load(host.clone(), session_id).await?;
        if running_status(&journal).is_some() {
            return Err(busy());
        }
        Ok(journal)
    }

    /// A turn a card started with its route decided (no router call): the message, the running
    /// status and the route entry, then the foreground flow from there.
    async fn start_card_turn(&self, ctx: Ctx, mut journal: Journal, session_id: &str, mode: &str, card: CardTurn, busy: &str) -> Result<TurnResult, RpcError> {
        let host = Host::new(ctx.clone());
        let run_id = new_run_id();
        let Some((stops, stage, sender)) = self.register(session_id, &run_id) else { return Err(RpcError::new(codes::BUSY, busy.to_owned())) };
        let data = self.inner.data.get();
        let mut message = json!({"text": card.text, "run_id": run_id});
        merge_fields(&mut message, card.message);
        journal.append(kinds::USER_MESSAGE, message).await?;
        let status_seq = journal
            .append(kinds::STATUS, json!({"state": "running", "run_id": run_id, "stage": card.stage, "mode": mode, "kind": "turn", "data_version": data.version()}))
            .await?;
        let detail = json!({"work": card.text, "route": card.route, "effort": data.effort.default, "by": "user"});
        let mut route = json!({"route": card.route, "stage": "route", "detail": detail, "raw": "", "run_id": run_id});
        merge_fields(&mut route, card.route_fields);
        journal.append(kinds::ROUTE, route).await?;
        let _ = host.session_status(session_id, SessionStatus::Running).await;
        let this = self.clone();
        let session_id = session_id.to_owned();
        let mode = mode.to_owned();
        let run = run_id.clone();
        self.spawn(async move {
            let outcome = this.drive_foreground(ctx, &session_id, run, status_seq, mode, stops, stage).await;
            let _ = sender.send(Some(outcome.map(|_| empty_result("done"))));
        });
        Ok(TurnResult { run_id, queued_as_steer: false })
    }

    /// The user's own edit of an open plan: a new version `by: user`, back to draft (no model call).
    async fn plan_edit(&self, ctx: Ctx, params: PlanEditParams) -> Result<Value, RpcError> {
        let host = Host::new(ctx);
        let mut journal = Journal::load(host.clone(), &params.session_id).await?;
        let entries = journal.entries_vec();
        let Some((seq, current)) = plan::version_of(&entries, &params.plan_id) else { return Err(RpcError::not_found("no such plan")) };
        if current.version != params.base_version {
            return Err(RpcError::new(codes::CONFLICT, format!("the plan changed since version {}: it is version {} now, and this edit was not applied", params.base_version, current.version))
                .with_data(json!({"version": current.version})));
        }
        let next = plan::user_edit(&current, &params).map_err(RpcError::invalid_params)?;
        let run_id = entries.iter().find(|entry| entry.seq == seq).map(|entry| str_field(&entry.data, "run_id").to_owned()).unwrap_or_default();
        let made = journal.append(chatflow_kinds::PLAN, next.entry(&run_id)).await?;
        Ok(json!({"version": next.version, "seq": made}))
    }

    /// Add a comment to an open plan (on the plan, or on one of its tasks), or take one back.
    async fn plan_comment(&self, ctx: Ctx, params: PlanCommentParams) -> Result<Value, RpcError> {
        let host = Host::new(ctx);
        let journal = Journal::load(host.clone(), &params.session_id).await?;
        if let Some(seq) = params.remove {
            let entry = journal.get(seq).filter(|entry| entry.kind == chatflow_kinds::PLAN_COMMENT).ok_or_else(|| RpcError::not_found("no such comment"))?;
            if str_field(&entry.data, "state") != "open" {
                return Err(RpcError::invalid_params("the comment was sent already"));
            }
            host.journal_set(&params.session_id, seq, "state", json!("removed")).await?;
            return Ok(json!({"seq": seq, "state": "removed"}));
        }
        let text = params.text.trim();
        if text.is_empty() {
            return Err(RpcError::invalid_params("a comment needs text"));
        }
        let Some((_, current)) = plan::version_of(&journal.entries_vec(), &params.plan_id) else { return Err(RpcError::not_found("no such plan")) };
        if !current.is_open() {
            return Err(RpcError::invalid_params(format!("the plan is {}", current.status)));
        }
        let mut data = json!({"plan_id": params.plan_id, "version": current.version, "text": text, "state": "open"});
        if let Some(n) = params.task {
            let task = current.tasks.iter().find(|task| task.n == n).ok_or_else(|| RpcError::invalid_params(format!("the plan has no task {n}")))?;
            data["task"] = json!(n);
            data["task_title"] = json!(task.title);
        }
        let seq = host.journal_append(&params.session_id, chatflow_kinds::PLAN_COMMENT, data).await?;
        Ok(json!({"seq": seq, "state": "open"}))
    }

    // ---- chatflow.steer / cancel / state ------------------------------------------------------

    async fn steer(&self, ctx: Ctx, params: SteerParams) -> Result<Value, RpcError> {
        let kind = if params.kind == "followup" { "followup" } else { "steer" };
        let host = Host::new(ctx.clone());
        let journal = Journal::load(host.clone(), &params.session_id).await?;
        // The run the steer goes to: driven here, or by another instance (its running status).
        let running = self.active(&params.session_id).map(|active| active.0).or_else(|| running_status(&journal).map(|status| str_field(&status.data, "run_id").to_owned()));
        let Some(run_id) = running else {
            // Nothing is running: the message starts a turn (with its mentions).
            let turn = TurnParams { session_id: params.session_id, text: params.text, mode: "auto".into(), attachments: params.attachments, spec: false, mentions: params.mentions };
            let result = self.turn(ctx, turn).await?;
            return Ok(json!({"queued": false, "run_id": result.run_id}));
        };
        let attached = self.prepare_attachments(&host, params.attachments).await?;
        // Other repositories the message references: opened read-only, named to the running run.
        let referenced = references::resolve(&host, &self.inner.data.get(), &params.session_id, &params.mentions, &attached).await?;
        let steer = references::with(attachments::with(json!({"text": params.text, "kind": kind, "target": "chat", "consumed": false}), &attached), &params.mentions);
        let seq = host.journal_append(&params.session_id, kinds::STEER, steer).await?;
        if let Some(mut note) = referenced {
            note["run_id"] = json!(run_id);
            note["steer_seq"] = json!(seq);
            host.journal_append(&params.session_id, kinds::NOTE, note).await?;
        }
        Ok(json!({"queued": true, "seq": seq}))
    }

    // ---- chatflow.attachment.put ----------------------------------------------------------------------

    /// Check a message's attachments and complete them (listings, dimensions, the vision hook).
    async fn prepare_attachments(&self, host: &Host, given: Vec<Attachment>) -> Result<Vec<Attachment>, RpcError> {
        if given.is_empty() {
            return Ok(given);
        }
        attachments::prepare(host, &self.inner.data.get(), given).await
    }

    /// Bytes go to the attachment store; a project path is referenced (see [`attachments::put`]).
    async fn attachment_put(&self, ctx: Ctx, params: AttachmentPutParams) -> Result<AttachmentPutResult, RpcError> {
        let host = Host::new(ctx.clone());
        let mut roots = Vec::new();
        if let Some(session_id) = params.session_id.as_deref().filter(|id| !id.is_empty()) {
            let session = host.session_get(session_id).await?;
            if !session.workspace.is_empty() {
                roots.push(PathBuf::from(&session.workspace));
            }
            roots.push(PathBuf::from(host.project_get(&session.project_id).await?.root));
        }
        if let Some(project_id) = params.project_id.as_deref().filter(|id| !id.is_empty()) {
            roots.push(PathBuf::from(host.project_get(project_id).await?.root));
        }
        let roots: Vec<PathBuf> = roots.into_iter().filter_map(|root| root.canonicalize().ok()).collect();
        let home = ctx.env.home.clone();
        let session_id = params.session_id.clone();
        let put = tokio::task::spawn_blocking(move || attachments::put(&home, &roots, params))
            .await
            .map_err(|error| RpcError::internal(format!("the attachment task failed: {error}")))??;
        // Its metadata in the graph, without holding up the message (memory may be starting).
        let recorded = put.clone();
        tokio::spawn(async move {
            if let Err(error) = attachments::record(&host, session_id.as_deref(), &recorded).await {
                tracing::warn!(error = %error.message, path = %recorded.path, "the attachment was not recorded in the graph");
            }
        });
        Ok(put)
    }

    /// A deleted session: its run stops, its prefill ends, and its lock file goes.
    async fn forget_session(&self, ctx: &Ctx, session_id: &str) {
        if let Some((_, stops, _, mut done)) = self.active(session_id) {
            stops.cancel.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(2), done.wait_for(|value| value.is_some())).await;
        }
        self.set_prewarm(session_id, None);
        if let Ok(mut warmed) = self.inner.warmed.lock() {
            warmed.remove(session_id);
        }
        locks::remove(&data_dir(ctx), session_id);
    }

    async fn cancel(&self, ctx: Ctx, params: SessionParams) -> Result<Value, RpcError> {
        if let Some((run_id, stops, _, mut done)) = self.active(&params.session_id) {
            stops.cancel.cancel();
            // Wait (briefly) for the run to acknowledge, so the caller sees the final status.
            let _ = tokio::time::timeout(Duration::from_secs(2), done.wait_for(|value| value.is_some())).await;
            return Ok(json!({"cancelled": true, "run_id": run_id}));
        }
        let host = Host::new(ctx.clone());
        let mut journal = Journal::load(host.clone(), &params.session_id).await?;
        let Some(status) = running_status(&journal) else {
            return Ok(json!({"cancelled": false, "reason": "nothing is running"}));
        };
        // Nobody drives it (no lock holder): close it here.
        if let Ok(Some(_lock)) = locks::try_lock(&data_dir(&ctx), &params.session_id) {
            let run_id = str_field(&status.data, "run_id").to_owned();
            journal.append(kinds::STATUS, json!({"state": "cancelled", "detail": "Cancelled.", "run_id": run_id})).await?;
            let _ = host.session_status(&params.session_id, SessionStatus::Cancelled).await;
            return Ok(json!({"cancelled": true, "run_id": run_id}));
        }
        Ok(json!({"cancelled": false, "reason": "the run is driven by another chat flow instance"}))
    }

    async fn state(&self, ctx: Ctx, params: SessionParams) -> Result<FlowState, RpcError> {
        let host = Host::new(ctx);
        let journal = Journal::load(host, &params.session_id).await?;
        // The journal says whether a run is going: a run's handle outlives its final status (it
        // still learns and records the session's status), and the UI asks again only when a status
        // entry lands, so a handle counted as running there would keep the run going in the UI.
        let status = running_status(&journal);
        let local = self.active(&params.session_id).filter(|_| status.is_some());
        let stage = match &local {
            Some((_, _, stage, _)) => stage.lock().ok().map(|stage| stage.clone()),
            None => status.as_ref().map(|entry| str_field(&entry.data, "stage").to_owned()),
        };
        Ok(FlowState {
            session_id: params.session_id,
            running: local.is_some() || status.is_some(),
            run_id: local.map(|active| active.0).or_else(|| status.map(|entry| str_field(&entry.data, "run_id").to_owned())),
            stage,
            pending_steers: journal.pending_steers("steer").len() as u32,
            pending_followups: journal.pending_steers("followup").len() as u32,
        })
    }

    // ---- chatflow.approve / background job events ---------------------------------------------

    async fn approve(&self, ctx: Ctx, params: ApproveParams) -> Result<Value, RpcError> {
        let host = Host::new(ctx);
        let entry = host
            .journal_read(&params.session_id, params.seq)
            .await?
            .into_iter()
            .find(|entry| entry.seq == params.seq)
            .ok_or_else(|| RpcError::not_found("no such journal entry"))?;
        if entry.kind != chatflow_kinds::APPROVAL {
            return Err(RpcError::invalid_params("that entry is not an approval"));
        }
        let current = str_field(&entry.data, "state").to_owned();
        if current != "pending" {
            return Ok(json!({"state": current, "changed": false}));
        }
        let state = if params.approve { "approved" } else { "denied" };
        host.journal_set(&params.session_id, params.seq, "state", json!(state)).await?;
        Ok(json!({"state": state, "changed": true}))
    }

    /// A background job of a session ended or matched its wake rule: tell the model (once per
    /// event, whichever instance hears it) and, when nothing runs in a chat session, wake it.
    async fn job_event(&self, ctx: Ctx, data: Value) -> Result<(), RpcError> {
        let session_id = str_field(&data, "session_id").to_owned();
        if session_id.is_empty() {
            return Ok(());
        }
        let host = Host::new(ctx.clone());
        let mut journal = Journal::load(host.clone(), &session_id).await?;
        let key = format!("{}:{}:{}", str_field(&data, "job_id"), str_field(&data, "state"), str_field(&data, "reason"));
        if journal.entries().any(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "job_key") == key) {
            return Ok(());
        }
        let set = self.inner.data.get();
        let text = job_event_text(&set, &data);
        let running = self.active(&session_id).is_some() || running_status(&journal).is_some();
        journal.append(kinds::NOTE, json!({"text": text, "purpose": "job", "host_message": true, "job_key": key, "job": data})).await?;
        if running {
            // The running loop reads it at its next step.
            return Ok(());
        }
        let session = host.session_get(&session_id).await?;
        if session.kind != SessionKind::Chat {
            return Ok(());
        }
        let run_id = new_run_id();
        let Some((stops, stage, sender)) = self.register(&session_id, &run_id) else { return Ok(()) };
        let status_seq = journal
            .append(kinds::STATUS, json!({"state": "running", "run_id": run_id, "stage": "act", "mode": "auto", "kind": "wake", "data_version": set.version()}))
            .await?;
        let _ = host.session_status(&session_id, SessionStatus::Running).await;
        let this = self.clone();
        self.spawn(async move {
            let outcome = this.drive_foreground(ctx, &session_id, run_id, status_seq, "auto".into(), stops, stage).await;
            let _ = sender.send(Some(outcome.map(|_| empty_result("done"))));
        });
        Ok(())
    }

    /// Delegated work needs its session's decision (an agent blocked, a need for the lead, the
    /// tree done): the session hears it (once, whichever instance hears the event) and, when
    /// nothing runs in it, wakes to decide with MANAGE (no router call). A running turn reads the
    /// note at its next step.
    async fn supervise_event(&self, ctx: Ctx, event: Supervise) -> Result<(), RpcError> {
        let host = Host::new(ctx.clone());
        let session_id = event.session_id.clone();
        let mut journal = Journal::load(host.clone(), &session_id).await?;
        if journal.entries().any(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "supervise_id") == event.id) {
            return Ok(());
        }
        let set = self.inner.data.get();
        let vars = [("agent", event.agent.as_str()), ("title", event.title.as_str()), ("text", event.text.trim())];
        let check = event.event == agent_events::CHECK;
        let text = match event.event.as_str() {
            agent_events::NEED => set.text("supervise_need", &vars),
            agent_events::TREE_DONE => set.text("supervise_tree_done", &vars),
            agent_events::CHECK => set.text("supervise_check", &[]),
            _ => set.text("supervise_blocked", &vars),
        };
        let running = self.active(&session_id).is_some() || running_status(&journal).is_some();
        // A check while the session works waits for the next one; it leaves nothing behind (the
        // MANAGE stage reads every agent's progress itself).
        if check && running {
            return Ok(());
        }
        let note = if check { json!({"text": "", "purpose": "supervise", "host_message": false, "supervise_id": event.id, "supervise": event}) } else {
            json!({"text": text, "purpose": "supervise", "host_message": true, "supervise_id": event.id, "supervise": event})
        };
        journal.append(kinds::NOTE, note).await?;
        if running {
            return Ok(());
        }
        let session = host.session_get(&session_id).await?;
        if session.kind != SessionKind::Chat {
            return Ok(());
        }
        // The session's own mode: a wake never acts with more freedom than the user gave it.
        let mode = journal
            .entries()
            .filter(|entry| entry.kind == kinds::STATUS)
            .filter_map(|entry| entry.data.get("mode").and_then(Value::as_str))
            .last()
            .unwrap_or("auto")
            .to_owned();
        let run_id = new_run_id();
        let Some((stops, stage, sender)) = self.register(&session_id, &run_id) else { return Ok(()) };
        let status_seq = journal
            .append(kinds::STATUS, json!({"state": "running", "run_id": run_id, "stage": "manage", "mode": mode, "kind": "turn", "data_version": set.version()}))
            .await?;
        let detail = json!({"work": text, "route": "MANAGE", "effort": set.effort.default, "by": "host", "check": check});
        journal
            .append(kinds::ROUTE, json!({"route": "MANAGE", "stage": "route", "detail": detail, "raw": "", "run_id": run_id}))
            .await?;
        let _ = host.session_status(&session_id, SessionStatus::Running).await;
        let this = self.clone();
        self.spawn(async move {
            let outcome = this.drive_foreground(ctx, &session_id, run_id, status_seq, mode, stops, stage).await;
            let _ = sender.send(Some(outcome.map(|_| empty_result("done"))));
        });
        Ok(())
    }

    /// A routine is due for a session (docs/SCHEDULES-PLAN.md). An agent check-in is a supervise
    /// `check` (MANAGE decides); any other wake tells the session the routine's brief (once,
    /// whichever instance hears it) and, when nothing runs in it, wakes it to do that.
    async fn routine_due(&self, ctx: Ctx, fired: Fired) -> Result<(), RpcError> {
        if fired.manage {
            let event = Supervise {
                id: fired.id.clone(),
                session_id: fired.session_id.clone(),
                project_id: fired.project_id.clone(),
                event: agent_events::CHECK.to_owned(),
                agent: String::new(),
                task_id: String::new(),
                title: String::new(),
                text: String::new(),
                failed_tests: None,
            };
            return self.supervise_event(ctx, event).await;
        }
        let session_id = fired.session_id.clone();
        let host = Host::new(ctx.clone());
        let mut journal = Journal::load(host.clone(), &session_id).await?;
        if journal.entries().any(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "routine_fire") == fired.id) {
            return Ok(());
        }
        let set = self.inner.data.get();
        // The routine's own words are the last line: they are the step.
        let text = set.text("routine_due", &[("title", fired.title.as_str()), ("when", fired.when.as_str()), ("text", fired.text.trim())]);
        let running = self.active(&session_id).is_some() || running_status(&journal).is_some();
        journal.append(kinds::NOTE, json!({"text": text, "purpose": "routine", "host_message": true, "routine_fire": fired.id, "routine_id": fired.routine_id})).await?;
        if running {
            // The running loop reads it at its next step.
            return Ok(());
        }
        let session = host.session_get(&session_id).await?;
        if session.kind != SessionKind::Chat {
            return Ok(());
        }
        // The session's own mode: a wake never acts with more freedom than the user gave it.
        let mode = journal
            .entries()
            .filter(|entry| entry.kind == kinds::STATUS)
            .filter_map(|entry| entry.data.get("mode").and_then(Value::as_str))
            .last()
            .unwrap_or("auto")
            .to_owned();
        let run_id = new_run_id();
        let Some((stops, stage, sender)) = self.register(&session_id, &run_id) else { return Ok(()) };
        let status_seq = journal
            .append(kinds::STATUS, json!({"state": "running", "run_id": run_id, "stage": "act", "mode": mode, "kind": "wake", "data_version": set.version()}))
            .await?;
        let _ = host.session_status(&session_id, SessionStatus::Running).await;
        let this = self.clone();
        self.spawn(async move {
            let outcome = this.drive_foreground(ctx, &session_id, run_id, status_seq, mode, stops, stage).await;
            let _ = sender.send(Some(outcome.map(|_| empty_result("done"))));
        });
        Ok(())
    }

    // ---- chatflow.agent_run -------------------------------------------------------------------

    async fn agent_run(&self, ctx: Ctx, call: &Call, params: AgentRunParams) -> Result<AgentRunResult, RpcError> {
        if let Some((_, _, _, mut done)) = self.active(&params.session_id) {
            // Already running here (a retried call): wait for that run.
            let outcome = tokio::select! {
                outcome = done.wait_for(|value| value.is_some()) => outcome.map(|value| value.clone()).ok().flatten(),
                _ = call.cancel.cancelled() => return Err(RpcError::cancelled()),
            };
            return outcome.unwrap_or_else(|| Err(RpcError::new(codes::UNAVAILABLE, "the run ended without a result").retryable()));
        }
        let run_id = new_run_id();
        let Some((stops, stage, sender)) = self.register(&params.session_id, &run_id) else {
            return Err(RpcError::new(codes::BUSY, "a run for this session just started").retryable());
        };
        // A caller that goes away (its plugin restarting) parks the run: the agents plugin's
        // successor calls again and the run resumes. Stopping an agent is `chatflow.cancel`.
        let caller_gone = stops.park.clone().drop_guard();
        let this = self.clone();
        let items = call.items.clone();
        let task_ctx = ctx.clone();
        let task_params = params.clone();
        let task_stops = stops.clone();
        let task = self.inner.tasks.spawn(async move {
            let outcome = this.drive_agent(task_ctx, task_params, run_id, task_stops, stage, items).await;
            let _ = sender.send(Some(outcome.clone()));
            outcome
        });
        let outcome = match task.await {
            Ok(outcome) => outcome,
            Err(error) => Err(RpcError::internal(format!("agent run task failed: {error}"))),
        };
        caller_gone.disarm();
        outcome
    }

    async fn drive_agent(
        &self,
        ctx: Ctx,
        params: AgentRunParams,
        new_run: String,
        stops: Stops,
        stage: Arc<Mutex<String>>,
        items: cc_protocol::ItemSink,
    ) -> Result<AgentRunResult, RpcError> {
        let host = Host::new(ctx.clone());
        let parked = || RpcError::new(codes::UNAVAILABLE, "the chat flow is restarting; call again to continue").retryable();
        let Some(_lock) = locks::lock(&data_dir(&ctx), &params.session_id, &stops.park).await.map_err(|error| RpcError::internal(error.to_string()))? else {
            return Err(parked());
        };
        let mut journal = Journal::load(host.clone(), &params.session_id).await?;
        // A finished run with nothing new to act on returns its recorded result.
        if let Some(last) = journal.last_status()
            && str_field(&last.data, "state") != "running"
            && str_field(&last.data, "task_id") == params.task_id
            && journal.pending_steers("steer").is_empty()
            && journal.pending_steers("followup").is_empty()
            && let Some(result) = last.data.get("result").and_then(|value| serde_json::from_value::<AgentRunResult>(value.clone()).ok())
        {
            return Ok(result);
        }
        let session = host.session_get(&params.session_id).await?;
        let mut task: Task = host.get(memory_methods::TASK_GET, json!({"id": params.task_id})).await?;
        let resumed = running_status(&journal);
        let (run_id, effort) = match &resumed {
            Some(status) => (
                str_field(&status.data, "run_id").to_owned(),
                schemas::effort_or_default(str_field(&status.data, "effort")),
            ),
            None => {
                let effort = params.options.get("effort").and_then(Value::as_str).map(schemas::effort_or_default).unwrap_or_else(|| "medium".into());
                (new_run, effort)
            }
        };
        let env = self.environment(&host, &mut journal, &session, &run_id, &params.mode, Priority::Agent, stops.clone(), stage).await?;
        if resumed.is_none() {
            // Pending input addressed to the agent becomes part of this run.
            let first_time = !journal.entries().any(|entry| entry.kind == kinds::USER_MESSAGE);
            if first_time {
                journal.append(kinds::USER_MESSAGE, json!({"text": task_message(&env.data, &task), "run_id": run_id, "task_id": task.id})).await?;
            }
            for followup in journal.pending_steers("followup") {
                journal.set(followup.seq, "as_turn", json!(true)).await?;
                journal.consume_steer(followup.seq).await?;
                journal
                    .append(kinds::USER_MESSAGE, references::carry(attachments::carry(json!({"text": str_field(&followup.data, "text"), "run_id": run_id, "from_steer": followup.seq}), &followup.data), &followup.data))
                    .await?;
            }
            journal
                .append(kinds::STATUS, json!({"state": "running", "run_id": run_id, "kind": "agent", "task_id": task.id, "effort": effort, "mode": params.mode, "data_version": env.data.version()}))
                .await?;
            let _ = host.session_status(&session.id, SessionStatus::Running).await;
            if first_time && !task.touches.is_empty() {
                // The graph-first start: read what the task touches before the first decision.
                let planned = touch_reads(&env.scope, &session.id, &task.touches, &env.data.limits);
                let mut done = HashSet::new();
                let any = stops.any();
                if let Err(error) = reads::run(&mut journal, &run_id, &env.scope, planned, &mut done, &any, env.data.limits.reads_per_round).await
                    && error.code != codes::CANCELLED
                {
                    return Err(error);
                }
            }
        }
        // Settle the contract against a baseline run in this workspace (once per run).
        if !task.contract.checks.is_empty() {
            let refined = crate::checks::refine(&env, &mut journal, &task.contract).await;
            match refined {
                Ok(contract) if contract != task.contract && task.contract_hash.is_none() => {
                    // Not claimed yet: the refinement is part of the contract that is frozen now.
                    host.call(memory_methods::TASK_UPDATE, json!({"id": task.id, "contract": contract, "freeze": true})).await?;
                    task.contract = contract;
                }
                Ok(contract) if contract != task.contract => {
                    // Claimed: the worker's refinement is only a proposal to the planner.
                    let proposer = session.agent.clone().unwrap_or_else(|| format!("session:{}", session.id));
                    let evidence = crate::checks::baseline_evidence(&journal, &run_id);
                    match crate::checks::submit(&env, &mut journal, &task, &contract, &proposer, &evidence, "before").await {
                        Ok(in_force) => task.contract = in_force,
                        Err(error) if error.code == codes::CANCELLED && stops.park.is_cancelled() => return Err(parked()),
                        Err(error) => return Err(error),
                    }
                }
                Ok(_) => {}
                Err(error) if error.code == codes::CANCELLED && stops.park.is_cancelled() => return Err(parked()),
                Err(error) => return Err(error),
            }
        }
        items.send(json!({"event": "running", "run_id": run_id, "session_id": session.id}));
        let name = session.agent.clone().unwrap_or_else(|| "agent".into());
        let spec = LoopSpec { kind: LoopKind::Agent { name }, effort, task: Some(task.clone()), step: None };
        let mut outcome = agent_loop::run(&env, &mut journal, &spec).await;
        // Follow-ups queued while working run after the work, in the same run.
        while let Ok(done) = &outcome {
            if done.outcome != "done" {
                break;
            }
            journal.sync().await?;
            let Some(followup) = journal.pending_steers("followup").into_iter().next() else { break };
            journal.set(followup.seq, "as_turn", json!(true)).await?;
            journal.consume_steer(followup.seq).await?;
            journal
                .append(kinds::USER_MESSAGE, references::carry(attachments::carry(json!({"text": str_field(&followup.data, "text"), "run_id": run_id, "from_steer": followup.seq}), &followup.data), &followup.data))
                .await?;
            outcome = agent_loop::run(&env, &mut journal, &spec).await;
        }
        // A run that was cancelled ends cancelled even when its caller went away too (parking
        // it would leave no end status, so it would read as running for ever).
        let cancelled = stops.cancel.is_cancelled();
        let outcome = match outcome {
            Ok(outcome) if outcome.outcome == "parked" && !cancelled => return Err(parked()),
            Ok(outcome) if outcome.outcome == "parked" => LoopOutcome { outcome: "cancelled".into(), summary: "Cancelled.".into(), ..outcome },
            Ok(outcome) => outcome,
            Err(error) if error.code == codes::CANCELLED && stops.park.is_cancelled() && !cancelled => return Err(parked()),
            Err(error) if error.code == codes::CANCELLED => LoopOutcome {
                outcome: "cancelled".into(),
                summary: "Cancelled.".into(),
                turns: 0,
                tool_calls: 0,
                changed_files: vec![],
                verified: None,
            },
            Err(error) => {
                journal.append(kinds::ERROR, json!({"message": error.message, "code": error.code, "run_id": run_id})).await?;
                LoopOutcome { outcome: "failed".into(), summary: error.message.clone(), turns: 0, tool_calls: 0, changed_files: vec![], verified: None }
            }
        };
        // The run goes into the graph (docs/PROJECT-KNOWLEDGE-PLAN.md P5); it cannot fail the run.
        let _ = crate::afteraction::record_run(&env, &journal, Some(&task.id), &outcome).await;
        let run_started_ms = journal
            .entries()
            .find(|entry| entry.kind == kinds::STATUS && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "state") == "running")
            .map(|entry| entry.ts_ms)
            .unwrap_or(0);
        // The run's end is a turn's end for the tree: what its commands left behind goes, its file
        // reservations free, and what it changed is recorded.
        if let Err(error) = env
            .host
            .call(cc_protocol::contracts::tools::methods::CLEANUP, json!({"workspace": env.scope.workspace, "session_id": params.session_id, "since_ms": run_started_ms}))
            .await
        {
            tracing::debug!(%error, "agent run cleanup failed");
        }
        if let Some(note) = crate::afteraction::changed_note(&env.host, &env.scope.workspace, &params.session_id, run_started_ms, &run_id).await {
            journal.append(kinds::NOTE, note).await?;
        }
        let result = AgentRunResult {
            outcome: outcome.outcome.clone(),
            summary: outcome.summary.clone(),
            turns: outcome.turns,
            tool_calls: outcome.tool_calls,
            changed_files: outcome.changed_files.clone(),
            usage: usage_of(&journal, &run_id),
        };
        journal
            .append(kinds::STATUS, json!({"state": outcome.outcome, "detail": outcome.summary, "run_id": run_id, "task_id": task.id, "result": result, "data_version": env.data.version()}))
            .await?;
        self.learn(&params.session_id, &run_id);
        let status = match outcome.outcome.as_str() {
            "done" => SessionStatus::Done,
            "cancelled" => SessionStatus::Cancelled,
            "blocked" | "stopped" => SessionStatus::Waiting,
            _ => SessionStatus::Failed,
        };
        let _ = host.session_status(&session.id, status).await;
        Ok(result)
    }

    // ---- lifecycle ------------------------------------------------------------------------------

    async fn resume_all(&self, ctx: Ctx) -> Result<(), RpcError> {
        let host = Host::new(ctx.clone());
        let sessions = host.session_list(None).await?;
        for session in sessions.into_iter().filter(|session| session.flow == FLOW && session.kind == SessionKind::Chat && matches!(session.status, SessionStatus::Running | SessionStatus::Waiting)) {
            let journal = Journal::load(host.clone(), &session.id).await?;
            let Some(status) = running_status(&journal) else { continue };
            let run_id = str_field(&status.data, "run_id").to_owned();
            let mode = match str_field(&status.data, "mode") {
                "" => "auto".to_owned(),
                mode => mode.to_owned(),
            };
            let Some((stops, stage, sender)) = self.register(&session.id, &run_id) else { continue };
            tracing::info!(session = %session.id, %run_id, "resuming a running turn from the journal");
            let this = self.clone();
            let ctx = ctx.clone();
            let status_seq = status.seq;
            self.spawn(async move {
                let outcome = this.drive_foreground(ctx, &session.id, run_id, status_seq, mode, stops, stage).await;
                let _ = sender.send(Some(outcome.map(|_| empty_result("done"))));
            });
        }
        Ok(())
    }
}

fn empty_result(outcome: &str) -> AgentRunResult {
    AgentRunResult { outcome: outcome.into(), summary: String::new(), turns: 0, tool_calls: 0, changed_files: vec![], usage: Value::Null }
}

/// Sum a run's journaled usage.
fn usage_of(journal: &Journal, run_id: &str) -> Value {
    let mut totals = [0u64; 4];
    let mut calls = 0u64;
    for entry in journal.entries().filter(|entry| entry.kind == kinds::USAGE && str_field(&entry.data, "run_id") == run_id) {
        calls += 1;
        for (index, field) in ["prompt_tokens", "cached_tokens", "completion_tokens", "wall_ms"].iter().enumerate() {
            totals[index] += entry.data.get(*field).and_then(Value::as_u64).unwrap_or(0);
        }
    }
    json!({"model_calls": calls, "prompt_tokens": totals[0], "cached_tokens": totals[1], "completion_tokens": totals[2], "model_ms": totals[3]})
}

/// The first message of an agent session: the task as the planner wrote it.
pub fn task_message(data: &Dataset, task: &Task) -> String {
    // A plan task's brief ends with its next step (write the change): it moves to the very end,
    // after the contract and the files, so the last line before the work is the step to take
    // (seen: briefs ending on "Touches" and "read first" with agents reading for an hour).
    let mut brief = task.brief.trim().to_owned();
    let mut next = String::new();
    for candidate in [data.text("plan_task_write", &[]), data.text("plan_task_write_files", &[("files", &task.touches.join(", "))])] {
        let candidate = candidate.trim().to_owned();
        if !candidate.is_empty() && brief.ends_with(&candidate) {
            brief.truncate(brief.len() - candidate.len());
            brief = brief.trim_end().to_owned();
            next = candidate;
            break;
        }
        // Skills follow the next step in a plan task's brief: the step is the line before them.
        if let Some(at) = brief.find(&candidate) {
            let rest = brief[at + candidate.len()..].to_owned();
            brief = format!("{}{}", brief[..at].trim_end(), rest);
            next = candidate;
            break;
        }
    }
    let mut text = data.text("task", &[("title", task.title.trim()), ("brief", brief.trim())]);
    if !task.plan.trim().is_empty() {
        text.push_str(&data.text("task_plan", &[("plan", task.plan.trim())]));
    }
    if !task.contract.checks.is_empty() {
        text.push_str(&data.text("task_contract", &[]));
        for check in &task.contract.checks {
            text.push_str(&data.text("task_check", &[("command", &check.spec)]));
        }
    }
    if !task.touches.is_empty() {
        text.push_str(&data.text("task_touches", &[("touches", &task.touches.join(", "))]));
    }
    if !next.is_empty() {
        text.push_str("\n\n");
        text.push_str(&next);
    }
    text
}

/// Reads for what a task touches: files are read, other names are resolved as symbols.
fn touch_reads(scope: &Scope, session_id: &str, touches: &[String], limits: &crate::data::Limits) -> Vec<ReadReq> {
    let mut files = Vec::new();
    let mut symbols = Vec::new();
    for touch in touches {
        // Structural: an existing path in the workspace is a file.
        if std::path::Path::new(&scope.workspace).join(touch).is_file() {
            files.push(touch.clone());
        } else {
            symbols.push(touch.clone());
        }
    }
    reads::plan(scope, &reads::Wanted { symbols: &symbols, files: &files, queries: &[], focus: &[], work: None, change: true, session_id, diagnostics: None }, limits)
}

// ---- probe: a real self-test of the flow's pure machinery ----------------------------------------

fn probe_checks(data_dir: &std::path::Path, layers: Vec<(String, PathBuf)>) -> Vec<ProbeCheck> {
    let mut checks = Vec::new();
    let shipped = Dataset::embedded();
    let mut check = |name: &str, run: &dyn Fn() -> Result<String, String>| {
        let started = Instant::now();
        let (ok, detail) = match run() {
            Ok(detail) => (true, detail),
            Err(detail) => (false, detail),
        };
        checks.push(ProbeCheck { name: name.into(), ok, detail, ms: started.elapsed().as_millis() as u64 });
    };
    check("data", &|| {
        let shipped = shipped.as_ref().map_err(|error| format!("the shipped data set is invalid: {error}"))?;
        // A broken edited set is reported, not fatal: the plugin keeps the shipped one.
        let layered = Data::new().map(|data| (data.set_layers(layers.clone()), data.get().version()));
        Ok(match layered {
            Ok((Loaded::Rejected(error), _)) => format!("shipped {}; edited set rejected: {error}", shipped.version()),
            Ok((_, version)) => format!("active {version}"),
            Err(error) => error,
        })
    });
    check("projection", &|| {
        let data = shipped.as_ref().map_err(String::clone)?;
        let entries = probe_journal();
        let messages = project(&entries, data);
        if messages != project(&entries, data) {
            return Err("projection is not deterministic".into());
        }
        let roles: Vec<&str> = messages.iter().map(|message| message.role.as_str()).collect();
        let expected = ["user", "user", "assistant", "tool", "user"];
        if roles != expected {
            return Err(format!("unexpected roles {roles:?}"));
        }
        Ok(format!("{} messages", messages.len()))
    });
    check("decisions", &|| {
        let router: schemas::RouterDecision = schemas::parse_json(
            r#"{"work":"w","route":"DO","effort":"low","symbols":["f"],"files":[],"queries":[],"focus":[]}"#,
        )?;
        let decision: schemas::Decision = schemas::parse_json(
            r#"{"missing":"","route":"DO","effort":"high","symbols":[],"files":[],"queries":[],"brief":"b","checks":["make test"],"note":"n"}"#,
        )?;
        if router.route != "DO" || decision.checks.len() != 1 {
            return Err("decisions parsed wrongly".into());
        }
        Ok("router and decide parse".into())
    });
    check("repetition", &|| {
        let mut detector = Detector::new(3);
        let signature = Detector::signature("read", &json!({"path": "a"}));
        let verdicts: Vec<Verdict> = (0..6).map(|_| detector.observe(&signature, "same")).collect();
        match (&verdicts[2], &verdicts[5]) {
            (Verdict::Replan(_), Verdict::Stop(_)) => Ok("replan then stop".into()),
            other => Err(format!("unexpected verdicts {other:?}")),
        }
    });
    check("session lock", &|| {
        let id = format!("probe-{}", uuid::Uuid::new_v4().simple());
        let first = locks::try_lock(data_dir, &id).map_err(|error| error.to_string())?.ok_or("lock busy")?;
        let second = locks::try_lock(data_dir, &id).map_err(|error| error.to_string())?;
        if second.is_some() {
            return Err("a held lock was granted twice".into());
        }
        let path = first.path().to_owned();
        drop(first);
        let _ = std::fs::remove_file(path);
        Ok("exclusive".into())
    });
    checks
}

fn probe_journal() -> Vec<JournalEntry> {
    let entry = |seq: u64, kind: &str, data: Value| JournalEntry { seq, ts_ms: 0, session_id: "probe".into(), kind: kind.into(), data };
    vec![
        entry(1, kinds::USER_MESSAGE, json!({"text": "fix"})),
        entry(2, kinds::ROUTE, json!({"stage": "route", "route": "DO", "detail": {"work": "fix"}, "run_id": "r"})),
        entry(3, kinds::TOOL_CALL, json!({"call_id": "c", "tool": "read", "raw_name": "read", "raw_arguments": "{}", "run_id": "r", "step": 1})),
        entry(4, kinds::TOOL_RESULT, json!({"call_id": "c", "ok": true, "output": "x", "run_id": "r"})),
        entry(5, kinds::NOTE, json!({"text": "impact", "host_message": true, "run_id": "r"})),
    ]
}

impl Plugin for Chatflow {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let this = self.clone();
        Box::pin(async move {
            // `chatflow-coding.turn` -> `turn`.
            let verb = call.method.split_once('.').map_or(call.method.as_str(), |(_, verb)| verb);
            match verb {
                verbs::TURN => to_value(&this.turn(ctx, parse_params(call.params.clone())?).await?),
                verbs::STEER => this.steer(ctx, parse_params(call.params.clone())?).await,
                verbs::CANCEL => this.cancel(ctx, parse_params(call.params.clone())?).await,
                verbs::STATE => to_value(&this.state(ctx, parse_params(call.params.clone())?).await?),
                verbs::RUNNING => to_value(&this.running()),
                verbs::APPROVE => this.approve(ctx, parse_params(call.params.clone())?).await,
                verbs::ATTACHMENT_PUT => to_value(&this.attachment_put(ctx, parse_params(call.params.clone())?).await?),
                verbs::KNOWLEDGE_ANSWER => this.knowledge_answer(ctx, parse_params(call.params.clone())?).await,
                verbs::PREWARM => to_value(&this.prewarm(ctx, parse_params(call.params.clone())?).await?),
                verbs::PLAN_UPDATE => this.plan_update(ctx, parse_params(call.params.clone())?).await,
                verbs::PLAN_IMPLEMENT => to_value(&this.plan_implement(ctx, parse_params(call.params.clone())?).await?),
                verbs::PLAN_REVISE => to_value(&this.plan_revise(ctx, parse_params(call.params.clone())?).await?),
                verbs::PLAN_EDIT => this.plan_edit(ctx, parse_params(call.params.clone())?).await,
                verbs::PLAN_COMMENT => this.plan_comment(ctx, parse_params(call.params.clone())?).await,
                verbs::AGENT_RUN => {
                    let params: AgentRunParams = parse_params(call.params.clone())?;
                    to_value(&this.agent_run(ctx, &call, params).await?)
                }
                _ => Err(RpcError::unknown_method(&call.method)),
            }
        })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        Box::pin(async move {
            let layers = Chatflow::data_layers(&ctx);
            Ok(ProbeReport::from_checks(probe_checks(&ctx.env.data_dir, layers)))
        })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let this = self.clone();
        Box::pin(async move {
            this.start_data(&ctx);
            // Learning from finished runs, first the ones no instance learned yet.
            let replayer: Arc<dyn crate::replay::Replayer> = Arc::new(FlowReplayer { flow: this.clone(), ctx: ctx.clone() });
            let paths = crate::replay::Paths::new(&data_dir(&ctx));
            let learner = this.inner.learner.get_or_init(|| crate::learn::Learner::start(Host::new(ctx.clone()), this.inner.data.clone(), this.inner.park.clone(), paths, replayer));
            learner.backlog();
            this.warm_projects(ctx.clone());
            // Resume in the background: activation must not wait for a predecessor's lock.
            let task_ctx = ctx.clone();
            tokio::spawn(async move {
                if let Err(error) = this.resume_all(task_ctx).await {
                    tracing::warn!(%error, "could not scan sessions to resume");
                }
            });
            Ok(())
        })
    }

    fn event(&self, ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        if topic == SKILLS_CHANGED {
            self.inner.skills_changed_ms.store(cc_protocol::contracts::now_ms(), Ordering::SeqCst);
        }
        if topic == SOURCE_CHANGED {
            // Read the live data again whatever the file times say (a merge can keep sizes and
            // times the watcher compares).
            let before = self.inner.data.get().version();
            let loaded = self.inner.data.reload(true);
            if !matches!(&loaded, crate::data::Loaded::Activated(version) if *version == before) {
                self.publish_data(&ctx, &loaded);
            }
        }
        let warm = match topic.as_str() {
            // The block holds the tools guide, the model's window, what memory knows and the
            // catalog of every plugin serving skills (language plugins come up last).
            frame_topics::PLUGIN_STATE => {
                let field = |name: &str| data.get(name).and_then(Value::as_str).unwrap_or("");
                field("state") == "active"
                    && (matches!(field("category"), "model" | "memory" | "tools") || cc_protocol::contracts::skills::listing_method(field("contract")).is_some())
            }
            frame_topics::PROJECT_CHANGED | SKILLS_CHANGED => true,
            cc_protocol::contracts::memory::topics::KNOWLEDGE_CHANGED => {
                data.get("kind").and_then(Value::as_str) == Some(cc_protocol::contracts::memory::knowledge::DESCRIPTION)
            }
            _ => false,
        };
        if warm {
            self.warm_projects(ctx.clone());
        }
        let this = self.clone();
        Box::pin(async move {
            if topic == tools_topics::JOB_EVENT {
                if let Err(error) = this.job_event(ctx, data).await {
                    tracing::warn!(%error, "could not deliver a background job event");
                }
            } else if topic == agents_topics::SUPERVISE {
                match serde_json::from_value::<Supervise>(data) {
                    Ok(event) => {
                        if let Err(error) = this.supervise_event(ctx, event).await {
                            tracing::warn!(%error, "could not tell a session about its agents");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "an unreadable supervision event"),
                }
            } else if topic == schedule_topics::FIRED {
                match serde_json::from_value::<Fired>(data) {
                    Ok(fired) => {
                        if let Err(error) = this.routine_due(ctx, fired).await {
                            tracing::warn!(%error, "could not wake a session for its routine");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "an unreadable routine event"),
                }
            } else if topic == frame_topics::SESSION_DELETED
                && let Ok(deleted) = serde_json::from_value::<SessionDeleted>(data)
            {
                this.forget_session(&ctx, &deleted.session_id).await;
            }
        })
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        let inner = self.inner.clone();
        Box::pin(async move {
            inner.park.cancel();
            inner.tasks.close();
            let _ = tokio::time::timeout(agent_loop::PARK_GRACE + Duration::from_millis(1500), inner.tasks.wait()).await;
        })
    }
}

/// A conversation's name from its first message: the first line's first words, at most 48
/// characters, cut at a word; markdown marks and trailing punctuation left out.
pub fn title_from(text: &str) -> String {
    const MAX: usize = 48;
    let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    let mut line = line.trim_start_matches(|c: char| matches!(c, '#' | '-' | '*' | '>' | ' '));
    // An opening courtesy says nothing about the conversation.
    for opener in ["please ", "pls ", "can you ", "could you ", "would you ", "hey ", "hi "] {
        if line.len() > opener.len() && line[..opener.len()].eq_ignore_ascii_case(opener) {
            line = line[opener.len()..].trim_start();
        }
    }
    let mut out = String::new();
    for word in line.split_whitespace() {
        let word = word.trim_matches(|c: char| matches!(c, '`' | '"' | '*' | '_' | '(' | ')'));
        if word.is_empty() {
            continue;
        }
        if out.is_empty() {
            out = word.chars().take(MAX).collect();
            continue;
        }
        if out.chars().count() + 1 + word.chars().count() > MAX {
            break;
        }
        out.push(' ');
        out.push_str(word);
    }
    // A cut never ends on a joining word or punctuation.
    let mut out = out.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '…')).to_owned();
    loop {
        let last = out.rsplit(' ').next().unwrap_or("").to_ascii_lowercase();
        let dangling = matches!(last.as_str(), "and" | "or" | "but" | "the" | "a" | "an" | "to" | "of" | "with" | "for" | "in" | "on" | "at" | "that" | "which" | "so" | "then" | "than" | "by" | "from" | "is" | "are" | "it");
        if !dangling || !out.contains(' ') {
            break;
        }
        out.truncate(out.len() - last.len());
        out = out.trim_end_matches(|c: char| c == ' ' || matches!(c, '.' | ',' | ';' | ':')).to_owned();
    }
    let mut chars = out.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
