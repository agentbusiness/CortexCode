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
    AgentRunParams, AgentRunResult, ApproveParams, Attachment, AttachmentPutParams, AttachmentPutResult, DecisionAnswerParams, FlowState, PlanRefParams, PlanUpdateParams,
    PrewarmParams, PrewarmResult, SessionParams, SpecUpdateParams, SteerParams, TurnParams, TurnResult, kinds as chatflow_kinds, methods, plan_status,
};
use crate::{plan, spec};
use cc_protocol::contracts::tools::topics as tools_topics;
use cc_protocol::contracts::frame::{
    JournalEntry, Session, SessionDeleted, SessionKind, SessionStatus, SessionUpdateParams, kinds, topics as frame_topics,
};
use cc_protocol::contracts::lifecycle::{Hello, ProbeCheck, ProbeReport};
use cc_protocol::contracts::memory::{Task, methods as memory_methods};
use cc_protocol::contracts::model::{Priority, methods as model_methods};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError, codes, parse_params, to_value};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub fn hello() -> Hello {
    let mut hello = Hello::new("cc-chatflow", env!("CARGO_PKG_VERSION"), "chatflow", "chatflow@1");
    hello.provides = [
        methods::TURN,
        methods::STEER,
        methods::CANCEL,
        methods::STATE,
        methods::AGENT_RUN,
        methods::APPROVE,
        methods::ATTACHMENT_PUT,
        methods::SPEC_UPDATE,
        methods::SPEC_SAVE,
        methods::PLAN_UPDATE,
        methods::PLAN_IMPLEMENT,
        methods::DECISION_ANSWER,
        methods::KNOWLEDGE_ANSWER,
        methods::PREWARM,
    ]
        .iter()
        .map(|method| (*method).to_owned())
        .collect();
    // Background jobs wake the session that started them; changed skills rebuild the prefixes; a
    // merge of the code folder (the source manager's start-up update) reloads the data set.
    hello.subscribes = vec![
        tools_topics::JOB_EVENT.to_owned(),
        SKILLS_CHANGED.to_owned(),
        frame_topics::SESSION_DELETED.to_owned(),
        SOURCE_CHANGED.to_owned(),
    ];
    hello
}

/// Published by every plugin that serves skills when its skills changed: the catalog in the
/// prefixes built before it is stale.
const SKILLS_CHANGED: &str = cc_protocol::contracts::skills::topics::CHANGED;

/// Published by the source manager when the code folder changed, among others once its start-up
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
    /// When a plugin's skills last changed (ms, 0: not since the start): a session's prefix built
    /// before it is rebuilt on its next turn.
    skills_changed_ms: AtomicU64,
    /// Learns every finished run, one at a time at idle priority (started on activation).
    learner: std::sync::OnceLock<crate::learn::Learner>,
}

#[derive(Clone)]
pub struct Chatflow {
    inner: Arc<Inner>,
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
                skills_changed_ms: AtomicU64::new(0),
                learner: std::sync::OnceLock::new(),
            }),
        })
    }

    /// The directory layers of the data set: `data_dir` from the plugin config (shipped files),
    /// then the live data in the plugin's folder of the code folder (taken once it exists).
    fn data_layers(ctx: &Ctx) -> Vec<(String, PathBuf)> {
        let mut layers = Vec::new();
        if let Some(dir) = ctx.config().get("data_dir").and_then(Value::as_str) {
            let dir = match dir.strip_prefix("~/") {
                Some(rest) => std::env::var("HOME").map(|home| PathBuf::from(home).join(rest)).unwrap_or_else(|_| PathBuf::from(dir)),
                None => PathBuf::from(dir),
            };
            layers.push(("shipped".to_owned(), dir));
        }
        if let Some(source) = &ctx.env.source_dir {
            layers.push((crate::data::LIVE.to_owned(), source.join("backend").join("data")));
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

fn new_run_id() -> String {
    format!("run-{}", uuid::Uuid::new_v4().simple())
}

fn data_dir(ctx: &Ctx) -> PathBuf {
    ctx.env.data_dir.clone()
}

/// Whether the last status of a journal says a run is in progress.
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
        let mut runs = self.lock_runs();
        if runs.get(session_id).is_some_and(|handle| handle.done.borrow().is_none()) {
            return None;
        }
        let stops = Stops { cancel: CancellationToken::new(), park: self.inner.park.child_token() };
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
        // The run's first wait is the code graph (opening the project, the repository map): say so.
        if let Ok(mut current) = stage.lock() {
            *current = "context".into();
        }
        let project = host.project_get(&session.project_id).await?;
        self.ensure_project_open(host, &project.id, &project.root).await;
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
        let host = Host::new(ctx.clone());
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
        let Some((stops, stage, sender)) = self.register(&params.session_id, &run_id) else {
            journal.append(kinds::STEER, steer).await?;
            let run_id = self.active(&params.session_id).map(|active| active.0).unwrap_or_default();
            if let Some(note) = note(&run_id) {
                journal.append(kinds::NOTE, note).await?;
            }
            return Ok(TurnResult { run_id, queued_as_steer: true });
        };
        let mut message = json!({"text": params.text, "run_id": run_id});
        if params.spec {
            // The user marked the message as the session's spec (the composer's Spec toggle).
            message["spec"] = json!(true);
        }
        if !params.mentions.is_empty() {
            message["mentions"] = json!(params.mentions);
        }
        journal.append(kinds::USER_MESSAGE, attachments::with(message, &attached)).await?;
        if let Some(note) = note(&run_id) {
            journal.append(kinds::NOTE, note).await?;
        }
        let status_seq = journal
            .append(kinds::STATUS, json!({"state": "running", "run_id": run_id, "stage": "route", "mode": params.mode, "kind": "turn", "data_version": self.inner.data.get().version()}))
            .await?;
        let _ = host.session_status(&params.session_id, SessionStatus::Running).await;
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
            }
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

    // ---- spec, decisions, plans ----------------------------------------------------------------

    /// The user's edit of the spec: a new version `by: user`.
    async fn spec_update(&self, ctx: Ctx, params: SpecUpdateParams) -> Result<Value, RpcError> {
        let host = Host::new(ctx);
        let mut journal = Journal::load(host.clone(), &params.session_id).await?;
        let entries = journal.entries_vec();
        let data = self.inner.data.get();
        let current = spec::current(&entries).map(|(_, spec)| spec);
        spec::check_base(current.as_ref(), params.base_version)?;
        let next = spec::user_update(&data, current.as_ref(), params.title.as_deref(), params.text.as_deref(), params.requirements.as_deref(), spec::accepted(&entries))
            .map_err(RpcError::invalid_params)?;
        let (seq, version) = spec::append(&mut journal, &next, None).await?;
        Ok(json!({"version": version, "seq": seq}))
    }

    /// Write the spec into the session's workspace as `docs/specs/<slug>.md`.
    async fn spec_save(&self, ctx: Ctx, params: SessionParams) -> Result<Value, RpcError> {
        let host = Host::new(ctx);
        let journal = Journal::load(host.clone(), &params.session_id).await?;
        let Some((_, current)) = spec::current(&journal.entries_vec()) else { return Err(RpcError::not_found("this conversation has no spec")) };
        let session = host.session_get(&params.session_id).await?;
        let root = if session.workspace.is_empty() { host.project_get(&session.project_id).await?.root } else { session.workspace.clone() };
        let relative = PathBuf::from("docs").join("specs").join(format!("{}.md", spec::slug(&current.title)));
        let path = PathBuf::from(&root).join(&relative);
        let data = self.inner.data.get();
        let text = spec::markdown(&data, &current);
        let written = path.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = written.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&written, text)
        })
        .await
        .map_err(|error| RpcError::internal(format!("the write task failed: {error}")))?
        .map_err(|error| RpcError::internal(format!("{}: {error}", path.display())))?;
        let shown = relative.to_string_lossy().to_string();
        host.journal_append(
            &params.session_id,
            kinds::NOTE,
            json!({"text": data.text("spec_saved", &[("path", &shown)]), "purpose": "spec_saved", "host_message": true, "path": shown, "version": current.version}),
        )
        .await?;
        Ok(json!({"path": path.to_string_lossy(), "relative": shown}))
    }

    /// Accept or reject a proposed decision; an accepted one joins the spec (a new version).
    async fn decision_answer(&self, ctx: Ctx, params: DecisionAnswerParams) -> Result<Value, RpcError> {
        let host = Host::new(ctx);
        let entry = host
            .journal_read(&params.session_id, params.seq)
            .await?
            .into_iter()
            .find(|entry| entry.seq == params.seq)
            .ok_or_else(|| RpcError::not_found("no such journal entry"))?;
        if entry.kind != chatflow_kinds::DECISION {
            return Err(RpcError::invalid_params("that entry is not a decision"));
        }
        let current = str_field(&entry.data, "state").to_owned();
        if current != spec::PROPOSED {
            return Ok(json!({"state": current, "changed": false}));
        }
        let state = if params.accept { spec::ACCEPTED } else { spec::REJECTED };
        host.journal_set(&params.session_id, params.seq, "state", json!(state)).await?;
        if params.accept {
            let mut journal = Journal::load(host.clone(), &params.session_id).await?;
            if let Some((_, current)) = spec::current(&journal.entries_vec()) {
                let decided = spec::Decided {
                    decided: str_field(&entry.data, "decided").to_owned(),
                    because: str_field(&entry.data, "because").to_owned(),
                    rejected: str_field(&entry.data, "rejected").to_owned(),
                    seq: Some(entry.seq),
                };
                let next = spec::with_decision(&self.inner.data.get(), &current, decided);
                spec::append(&mut journal, &next, None).await?;
            }
            // The decision is project knowledge too (owner decision, docs/PROJECT-KNOWLEDGE-IDEAS.md
            // §11): a `decision` item in the graph; the entry keeps its id. Memory being away never
            // fails the answer.
            let decided = str_field(&entry.data, "decided").trim().to_owned();
            let because = str_field(&entry.data, "because").trim().to_owned();
            let rejected = str_field(&entry.data, "rejected").trim().to_owned();
            let mut text = decided.clone();
            if !because.is_empty() {
                text.push_str(&format!(" Because {}", because.trim_end_matches('.')));
                text.push('.');
            }
            if !rejected.is_empty() {
                text.push_str(&format!(" Rejected: {rejected}"));
            }
            let item = json!({
                "kind": "decision", "name": knowledge_name(&decided), "text": text, "source": "user", "by": "user",
                "evidence": [format!("journal://{}/{}", params.session_id, params.seq)]
            });
            self.remember(&host, &params.session_id, params.seq, item).await;
        }
        Ok(json!({"state": state, "changed": true}))
    }

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
        if current != spec::PROPOSED {
            return Ok(json!({"state": current, "changed": false}));
        }
        let state = if params.accept { spec::ACCEPTED } else { spec::REJECTED };
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
        let busy = || RpcError::new(codes::BUSY, "a run is going in this conversation; implement the plan when it ends");
        if self.active(&params.session_id).is_some() {
            return Err(busy());
        }
        let mut journal = Journal::load(host.clone(), &params.session_id).await?;
        if running_status(&journal).is_some() {
            return Err(busy());
        }
        let Some((_, current)) = plan::version_of(&journal.entries_vec(), &params.plan_id) else { return Err(RpcError::not_found("no such plan")) };
        if !current.is_open() {
            return Err(RpcError::invalid_params(format!("the plan is {}", current.status)));
        }
        let run_id = new_run_id();
        let Some((stops, stage, sender)) = self.register(&params.session_id, &run_id) else { return Err(busy()) };
        let data = self.inner.data.get();
        let text = data.text("implement_request", &[]);
        journal.append(kinds::USER_MESSAGE, json!({"text": text, "run_id": run_id, "implement": params.plan_id})).await?;
        let status_seq = journal
            .append(kinds::STATUS, json!({"state": "running", "run_id": run_id, "stage": "implement", "mode": params.mode, "kind": "turn", "data_version": data.version()}))
            .await?;
        let detail = json!({"work": text, "route": "IMPLEMENT", "effort": data.effort.default, "by": "user"});
        journal
            .append(kinds::ROUTE, json!({"route": "IMPLEMENT", "stage": "route", "detail": detail, "raw": "", "run_id": run_id, "plan_id": params.plan_id, "spec_action": "none", "settles": false}))
            .await?;
        let _ = host.session_status(&params.session_id, SessionStatus::Running).await;
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
        tokio::task::spawn_blocking(move || attachments::put(&home, &roots, params))
            .await
            .map_err(|error| RpcError::internal(format!("the attachment task failed: {error}")))?
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
        let local = self.active(&params.session_id);
        let status = running_status(&journal);
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
        let outcome = match outcome {
            Ok(outcome) if outcome.outcome == "parked" => return Err(parked()),
            Ok(outcome) => outcome,
            Err(error) if error.code == codes::CANCELLED && stops.park.is_cancelled() => return Err(parked()),
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
        for session in sessions.into_iter().filter(|session| session.kind == SessionKind::Chat && matches!(session.status, SessionStatus::Running | SessionStatus::Waiting)) {
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
    let mut text = data.text("task", &[("title", task.title.trim()), ("brief", task.brief.trim())]);
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
            match call.method.as_str() {
                methods::TURN => to_value(&this.turn(ctx, parse_params(call.params.clone())?).await?),
                methods::STEER => this.steer(ctx, parse_params(call.params.clone())?).await,
                methods::CANCEL => this.cancel(ctx, parse_params(call.params.clone())?).await,
                methods::STATE => to_value(&this.state(ctx, parse_params(call.params.clone())?).await?),
                methods::APPROVE => this.approve(ctx, parse_params(call.params.clone())?).await,
                methods::ATTACHMENT_PUT => to_value(&this.attachment_put(ctx, parse_params(call.params.clone())?).await?),
                methods::SPEC_UPDATE => this.spec_update(ctx, parse_params(call.params.clone())?).await,
                methods::SPEC_SAVE => this.spec_save(ctx, parse_params(call.params.clone())?).await,
                methods::DECISION_ANSWER => this.decision_answer(ctx, parse_params(call.params.clone())?).await,
                methods::KNOWLEDGE_ANSWER => this.knowledge_answer(ctx, parse_params(call.params.clone())?).await,
                methods::PREWARM => to_value(&this.prewarm(ctx, parse_params(call.params.clone())?).await?),
                methods::PLAN_UPDATE => this.plan_update(ctx, parse_params(call.params.clone())?).await,
                methods::PLAN_IMPLEMENT => to_value(&this.plan_implement(ctx, parse_params(call.params.clone())?).await?),
                methods::AGENT_RUN => {
                    let params: AgentRunParams = parse_params(call.params.clone())?;
                    to_value(&this.agent_run(ctx, &call, params).await?)
                }
                other => Err(RpcError::unknown_method(other)),
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
            let learner = this.inner.learner.get_or_init(|| crate::learn::Learner::start(Host::new(ctx.clone()), this.inner.data.clone(), this.inner.park.clone()));
            learner.backlog();
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
        let this = self.clone();
        Box::pin(async move {
            if topic == tools_topics::JOB_EVENT {
                if let Err(error) = this.job_event(ctx, data).await {
                    tracing::warn!(%error, "could not deliver a background job event");
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

/// A knowledge item's name from a decision's statement: its first words, at most 60 characters,
/// cut at a word.
fn knowledge_name(statement: &str) -> String {
    let mut name = String::new();
    for word in statement.split_whitespace() {
        if !name.is_empty() && name.len() + 1 + word.len() > 60 {
            break;
        }
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
    }
    if name.is_empty() { statement.chars().take(60).collect() } else { name }
}
