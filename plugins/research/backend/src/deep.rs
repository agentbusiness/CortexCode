//! `research.deep`: deep research as a background job, built so a ~32k-token context is enough for
//! dozens of sources: the model never sees more than one chunk of one source, or a bounded batch of
//! notes, at a time.
//!
//! 1. **plan**: one model call (JSON schema) turns the topic (and the caller's questions, if any)
//!    into sub-questions, each with search queries and the sources to ask;
//! 2. **search**: every query on its sources; candidates are ranked by how many queries found them
//!    (then spread across sources), skipping sources the topic's notes already cite (notes are
//!    global, kept by the memory plugin: research on the topic from any project counts);
//! 3. **read** (map): each picked source is fetched as text and split into chunks of `chunk_chars`;
//!    one model call per chunk extracts notes (claim + verbatim quote) for the questions. A quote
//!    that is not in the chunk is dropped, so every note is grounded in what was read;
//! 4. **synthesize** (reduce): the topic's notes only (never raw pages) become a summary and
//!    findings citing notes; more notes than fit are condensed in batches first;
//! 5. **gap check**: a model call names unanswered questions with new queries: another round, up to
//!    `depth` rounds;
//! 6. **report**: `docs/research/<topic-slug>.md` in the project, written from the synthesis and the
//!    notes (sources with dates, confidence, open questions).
//!
//! The job's state is saved after every step (`<data>/jobs/<id>.json`); a job interrupted by a
//! restart or cancelled resumes from it, and sources its notes cite are not read again.

use crate::config::Config;
use crate::notes::Notes;
use crate::sources::url_key;
use crate::text::{match_form, quote_in, squash};
use cc_protocol::contracts::model::{ChatMessage, ChatParams, ChatResult, Priority, Sampling, Thinking};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::research::{
    DeepParams, FetchParams, FetchResult, Job, Note, NoteAddParams, Progress, SearchHit, SearchParams, SearchResult, sources, states, topics,
};
use cc_protocol::{BoxFuture, RpcError, codes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// What a deep job uses: search, fetch, the model and the project registry.
pub trait Tools: Send + Sync {
    fn search(&self, params: SearchParams, cancel: CancellationToken) -> BoxFuture<'static, Result<SearchResult, RpcError>>;
    fn fetch(&self, params: FetchParams) -> BoxFuture<'static, Result<FetchResult, RpcError>>;
    fn chat(&self, params: ChatParams, cancel: CancellationToken) -> BoxFuture<'static, Result<ChatResult, RpcError>>;
    /// The project's root directory (the report goes under it).
    fn project_root(&self, project_id: &str) -> BoxFuture<'static, Result<String, RpcError>>;
}

pub type Publish = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// A sub-question with its searches.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Planned {
    pub question: String,
    pub queries: Vec<String>,
    pub sources: Vec<String>,
}

/// A source picked for reading.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    pub hit: SearchHit,
    /// What is fetched (a paper's PDF rather than its landing page).
    pub read_url: String,
    /// Indexes into the job's questions.
    pub questions: Vec<usize>,
    pub found_by: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Unread {
    pub title: String,
    pub url: String,
    pub why: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    pub statement: String,
    pub notes: Vec<String>,
    pub confidence: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Synthesis {
    pub summary: String,
    pub findings: Vec<Finding>,
    pub open_questions: Vec<String>,
}

/// Everything needed to continue a job.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobState {
    pub job: Job,
    pub params: DeepParams,
    /// The current round's sub-questions (to search), empty once searched.
    pub pending: Vec<Planned>,
    pub candidates: Vec<Candidate>,
    /// URL keys whose extraction completed successfully (all rounds).
    pub visited: Vec<String>,
    pub unread: Vec<Unread>,
    pub queries_run: Vec<String>,
    pub sources_searched: Vec<String>,
    pub synthesis: Option<Synthesis>,
    /// Model calls whose answer could not be used, and quotes not found in their source.
    pub dropped_quotes: u32,
}

/// Reading a source can fail before extraction starts, or while extracting/storing evidence.
/// Keep this origin explicit: an HTTP error code can also come from the model or memory.
enum ReadFailure {
    Source(RpcError),
    Extraction(RpcError),
}

impl From<RpcError> for ReadFailure {
    fn from(error: RpcError) -> Self { Self::Extraction(error) }
}

struct Worker {
    generation: uuid::Uuid,
    cancel: CancellationToken,
}

pub struct Jobs {
    dir: PathBuf,
    tools: Arc<dyn Tools>,
    notes: Arc<Notes>,
    config: Config,
    publish: Publish,
    running: Mutex<HashMap<String, Worker>>,
    /// Set while handing running jobs to a successor (a hot swap): they stop as `interrupted`.
    handing_off: std::sync::atomic::AtomicBool,
}

fn phase_names() -> &'static [&'static str] {
    &["plan", "search", "read", "synthesize", "gap", "report", "done"]
}

impl Jobs {
    pub fn new(dir: PathBuf, tools: Arc<dyn Tools>, notes: Arc<Notes>, config: Config, publish: Publish) -> Self {
        Self { dir, tools, notes, config, publish, running: Mutex::new(HashMap::new()), handing_off: std::sync::atomic::AtomicBool::new(false) }
    }

    fn path(&self, job_id: &str) -> PathBuf {
        self.dir.join(format!("{job_id}.json"))
    }

    fn load(&self, job_id: &str) -> Option<JobState> {
        let safe = job_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !safe {
            return None;
        }
        serde_json::from_str(&std::fs::read_to_string(self.path(job_id)).ok()?).ok()
    }

    fn save(&self, state: &JobState) {
        let write = || -> Result<(), String> {
            std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
            let tmp = self.dir.join(format!(".{}.tmp", state.job.job_id));
            std::fs::write(&tmp, serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, self.path(&state.job.job_id)).map_err(|e| e.to_string())
        };
        if let Err(error) = write() {
            tracing::warn!("research: cannot save job {}: {error}", state.job.job_id);
        }
    }

    fn all(&self) -> Vec<JobState> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut jobs: Vec<JobState> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".json")?;
                if id.starts_with('.') {
                    return None;
                }
                self.load(id)
            })
            .collect();
        jobs.sort_by(|a, b| b.job.started_ms.cmp(&a.job.started_ms));
        jobs
    }

    /// Jobs saved as running by a previous process are interrupted (call `research.deep` again on
    /// the topic to resume, or a successor resumes them after a hot swap).
    pub fn mark_interrupted(&self) {
        let Ok(running) = self.running.lock() else { return };
        for mut state in self.all() {
            if state.job.state == states::RUNNING && !running.contains_key(&state.job.job_id) {
                state.job.state = states::INTERRUPTED.into();
                state.job.updated_ms = now_ms();
                self.save(&state);
            }
        }
    }

    pub fn running_ids(&self) -> Vec<String> {
        self.running.lock().map(|r| r.keys().cloned().collect()).unwrap_or_default()
    }

    /// Stop the running jobs for a successor (hot swap): each is saved as `interrupted` at its last
    /// step; returns their ids once they have stopped (at most `limit`).
    pub async fn hand_off(&self, limit: std::time::Duration) -> Vec<String> {
        let ids = self.running_ids();
        self.handing_off.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(running) = self.running.lock() {
            for worker in running.values() {
                worker.cancel.cancel();
            }
        }
        let deadline = tokio::time::Instant::now() + limit;
        while !self.running_ids().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        ids
    }

    pub fn status(&self, job_id: &str) -> Result<Job, RpcError> {
        self.load(job_id).map(|state| state.job).ok_or_else(|| RpcError::not_found(format!("no research job {job_id}")))
    }

    pub fn list(&self, project_id: Option<&str>) -> Vec<Job> {
        self.all().into_iter().map(|state| state.job).filter(|job| project_id.is_none_or(|p| job.project_id == p)).collect()
    }

    pub fn cancel(&self, job_id: &str) -> Result<Job, RpcError> {
        // The same ownership guard serializes cancellation with spawn and worker checkpoints.
        let running = self.running.lock().map_err(|_| RpcError::internal("research job ownership unavailable"))?;
        let mut state = self.load(job_id).ok_or_else(|| RpcError::not_found(format!("no research job {job_id}")))?;
        if let Some(worker) = running.get(job_id) {
            worker.cancel.cancel();
        }
        if state.job.state == states::RUNNING || state.job.state == states::INTERRUPTED {
            state.job.state = states::CANCELLED.into();
            state.job.updated_ms = now_ms();
            self.save(&state);
            (self.publish)(topics::JOB, json!(state.job));
        }
        Ok(state.job)
    }

    /// Start a job, or resume the topic's unfinished one.
    pub fn start(self: &Arc<Self>, params: DeepParams) -> Result<Job, RpcError> {
        if params.project_id.trim().is_empty() || params.topic.trim().is_empty() {
            return Err(RpcError::invalid_params("research.deep needs project_id and topic"));
        }
        if let Some(bad) = params.sources.iter().find(|s| !sources::ALL.contains(&s.as_str())) {
            return Err(RpcError::invalid_params(format!("unknown source `{bad}`; sources: {}", sources::ALL.join(", "))));
        }
        let topic_slug = crate::text::slug(&params.topic);
        let same_topic = |state: &JobState| state.job.project_id == params.project_id && crate::text::slug(&state.job.topic) == topic_slug;
        let previous = self.all().into_iter().filter(|s| same_topic(s)).collect::<Vec<_>>();
        if let Some(active) = previous.iter().find(|s| s.job.state == states::RUNNING && self.running_ids().contains(&s.job.job_id)) {
            return Ok(active.job.clone());
        }
        let unfinished = previous.into_iter().find(|s| matches!(s.job.state.as_str(), states::INTERRUPTED | states::CANCELLED | states::FAILED | states::RUNNING));
        let state = match unfinished {
            Some(mut state) if !params.fresh => {
                state.job.resumed += 1;
                state.job.state = states::RUNNING.into();
                state.job.error = None;
                state.job.updated_ms = now_ms();
                if params.session_id.is_some() {
                    state.job.session_id = params.session_id.clone();
                }
                state
            }
            _ => {
                let depth = params.depth.unwrap_or(self.config.deep_depth).clamp(1, 4);
                let now = now_ms();
                JobState {
                    job: Job {
                        job_id: format!("rj-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
                        project_id: params.project_id.clone(),
                        topic: squash(&params.topic),
                        state: states::RUNNING.into(),
                        phase: "plan".into(),
                        round: 0,
                        depth,
                        questions: params.questions.iter().map(|q| squash(q)).filter(|q| !q.is_empty()).collect(),
                        session_id: params.session_id.clone(),
                        started_ms: now,
                        updated_ms: now,
                        ..Job::default()
                    },
                    params: params.clone(),
                    ..JobState::default()
                }
            }
        };
        self.spawn(state)
    }

    /// Resume a job by id (after a hot swap).
    pub fn resume(self: &Arc<Self>, job_id: &str) -> Option<Job> {
        let mut state = self.load(job_id)?;
        if !matches!(state.job.state.as_str(), states::RUNNING | states::INTERRUPTED) {
            return None;
        }
        state.job.state = states::RUNNING.into();
        state.job.resumed += 1;
        self.spawn(state).ok()
    }

    fn spawn(self: &Arc<Self>, mut state: JobState) -> Result<Job, RpcError> {
        let token = CancellationToken::new();
        let generation = uuid::Uuid::new_v4();
        let job;
        {
            let mut running = self.running.lock().map_err(|_| RpcError::internal("research job ownership unavailable"))?;
            if let Some(previous) = running.get(&state.job.job_id)
                && !previous.cancel.is_cancelled() {
                return self.status(&state.job.job_id);
            }
            // Refresh after taking ownership: another worker may have checkpointed since the
            // caller read the saved job. Preserve the caller's new session and resume intent.
            if let Some(mut saved) = self.load(&state.job.job_id) {
                saved.job.resumed = saved.job.resumed.max(state.job.resumed);
                saved.job.session_id = state.job.session_id.clone();
                saved.job.state = states::RUNNING.into();
                saved.job.error = None;
                saved.job.updated_ms = now_ms();
                state = saved;
            }
            job = state.job.clone();
            // Superseding a cancelled generation never lets its late completion touch this one.
            if let Some(previous) = running.insert(job.job_id.clone(), Worker { generation, cancel: token.clone() }) {
                previous.cancel.cancel();
            }
            self.save(&state);
            (self.publish)(topics::JOB, json!(job));
        }
        let jobs = self.clone();
        tokio::spawn(async move {
            let id = state.job.job_id.clone();
            let mut run = Run { jobs: jobs.clone(), state, cancel: token.clone(), generation };
            let outcome = run.drive().await;
            let mut state = run.state;
            let Ok(mut running) = jobs.running.lock() else { return };
            if !running.get(&id).is_some_and(|worker| worker.generation == generation) {
                return;
            }
            // A cancel may have been saved meanwhile; it wins over whatever the run reached.
            let cancelled = token.is_cancelled() || jobs.load(&id).is_some_and(|saved| saved.job.state == states::CANCELLED);
            let handed_off = jobs.handing_off.load(std::sync::atomic::Ordering::SeqCst);
            match outcome {
                _ if handed_off => state.job.state = states::INTERRUPTED.into(),
                _ if cancelled => {
                    state.job.state = states::CANCELLED.into();
                    run_progress(&jobs, &state, "cancelled", 0, 0, "cancelled; its notes are kept and `research.deep` on the topic resumes it");
                }
                Ok(()) => state.job.state = states::DONE.into(),
                Err(error) => {
                    state.job.state = states::FAILED.into();
                    state.job.error = Some(error.message.clone());
                    run_progress(&jobs, &state, "failed", 0, 0, &error.message);
                }
            }
            state.job.updated_ms = now_ms();
            jobs.save(&state);
            running.remove(&id);
            (jobs.publish)(topics::JOB, json!(state.job));
        });
        Ok(job)
    }
}

fn run_progress(jobs: &Jobs, state: &JobState, phase: &str, done: u32, total: u32, detail: &str) {
    let progress = Progress {
        job_id: state.job.job_id.clone(),
        project_id: state.job.project_id.clone(),
        topic: state.job.topic.clone(),
        phase: phase.into(),
        round: state.job.round,
        done,
        total,
        detail: detail.into(),
        session_id: state.job.session_id.clone(),
        ts_ms: now_ms(),
    };
    (jobs.publish)(topics::PROGRESS, json!(progress));
}

struct Run {
    jobs: Arc<Jobs>,
    state: JobState,
    cancel: CancellationToken,
    generation: uuid::Uuid,
}

impl Run {
    fn config(&self) -> &Config {
        &self.jobs.config
    }

    fn progress(&self, phase: &str, done: u32, total: u32, detail: impl Into<String>) {
        if let Ok(running) = self.jobs.running.lock()
            && running.get(&self.state.job.job_id).is_some_and(|worker| worker.generation == self.generation && !worker.cancel.is_cancelled()) {
            run_progress(&self.jobs, &self.state, phase, done, total, &detail.into());
        }
    }

    fn checkpoint(&mut self, phase: &str) {
        self.state.job.phase = phase.into();
        self.state.job.updated_ms = now_ms();
        if let Ok(running) = self.jobs.running.lock()
            && running.get(&self.state.job.job_id).is_some_and(|worker| worker.generation == self.generation && !worker.cancel.is_cancelled()) {
            self.jobs.save(&self.state);
        }
    }

    fn check(&self) -> Result<(), RpcError> {
        if self.cancel.is_cancelled() { Err(RpcError::cancelled()) } else { Ok(()) }
    }

    /// The topic's notes: global, so research on the topic from any project counts.
    async fn topic_notes(&self) -> Result<Vec<Note>, RpcError> {
        self.jobs.notes.list("", Some(&self.state.job.topic)).await
    }

    async fn drive(&mut self) -> Result<(), RpcError> {
        debug_assert!(phase_names().contains(&self.state.job.phase.as_str()));
        loop {
            self.check()?;
            match self.state.job.phase.as_str() {
                "plan" => {
                    self.plan().await?;
                    self.state.job.round = self.state.job.round.max(1);
                    self.checkpoint("search");
                }
                "search" => {
                    self.search().await?;
                    self.checkpoint("read");
                }
                "read" => {
                    self.read().await?;
                    self.checkpoint("synthesize");
                }
                "synthesize" => {
                    self.synthesize().await?;
                    self.checkpoint("gap");
                }
                "gap" => {
                    if self.state.job.round < self.state.job.depth && self.gaps().await? {
                        self.state.job.round += 1;
                        self.checkpoint("search");
                    } else {
                        self.checkpoint("report");
                    }
                }
                "report" => {
                    self.report().await?;
                    self.checkpoint("done");
                }
                _ => return Ok(()),
            }
        }
    }

    // ----- model -----

    async fn ask(&self, system: &str, user: String, schema: Value) -> Result<Value, RpcError> {
        self.check()?;
        let priority = match self.config().model_priority.as_str() {
            "foreground" => Priority::Foreground,
            "background" => Priority::Background,
            _ => Priority::Agent,
        };
        let params = ChatParams {
            messages: vec![ChatMessage::new("system", system), ChatMessage::new("user", user)],
            tools: Vec::new(),
            tool_choice: None,
            grammar: None,
            json_schema: Some(schema),
            sampling: Sampling { temperature: 0.3, top_p: 0.9, ..Sampling::default() },
            thinking: Thinking::default(),
            priority,
            cache_key: Some(format!("research-{}", self.state.job.job_id)),
            stop: Vec::new(),
            max_tokens: Some(self.config().model_max_tokens),
            idle_only: false,
        };
        let result = self.jobs.tools.chat(params, self.cancel.clone()).await.map_err(|error| {
            if error.code == codes::CANCELLED {
                error
            } else {
                RpcError::new(&error.code, format!("the model: {}", error.message))
            }
        })?;
        self.check()?;
        parse_json_answer(&result.content).ok_or_else(|| RpcError::internal(format!("the model's answer is not JSON: {}", crate::text::clip(&result.content, 200))))
    }

    fn questions_block(&self) -> String {
        self.state.job.questions.iter().enumerate().map(|(i, q)| format!("{}. {q}", i + 1)).collect::<Vec<_>>().join("\n")
    }

    // ----- plan -----

    async fn plan(&mut self) -> Result<(), RpcError> {
        if !self.state.pending.is_empty() {
            return Ok(());
        }
        self.progress("plan", 0, 1, "planning sub-questions and searches");
        let known: Vec<String> = self.topic_notes().await?.iter().take(30).map(|n| format!("- {}", n.claim)).collect();
        let given = &self.state.job.questions;
        let mut user = format!("Topic: {}\nToday: {}\n", self.state.job.topic, chrono::Utc::now().date_naive());
        if given.is_empty() {
            user.push_str("Write 2 to 5 sub-questions that together answer the topic.\n");
        } else {
            user.push_str(&format!("Use exactly these questions, in this order:\n{}\n", self.questions_block()));
        }
        if let Some(since) = &self.state.params.since {
            user.push_str(&format!("Only sources from {since} on matter.\n"));
        }
        if !known.is_empty() {
            user.push_str(&format!("Already known from earlier research (search for what is missing):\n{}\n", known.join("\n")));
        }
        user.push_str(SOURCE_GUIDE);
        let answer = self.ask(PLAN_SYSTEM, user, plan_schema(if given.is_empty() { 5 } else { given.len().max(1) })).await?;
        let mut planned = planned_from(&answer);
        if planned.is_empty() {
            return Err(RpcError::internal("the model planned no questions"));
        }
        if !given.is_empty() {
            for (slot, question) in planned.iter_mut().zip(given.iter()) {
                slot.question = question.clone();
            }
            planned.truncate(given.len());
        } else {
            self.state.job.questions = planned.iter().map(|p| p.question.clone()).collect();
        }
        self.apply_source_override(&mut planned);
        self.progress("plan", 1, 1, format!("{} sub-question(s): {}", planned.len(), planned.iter().map(|p| p.question.as_str()).collect::<Vec<_>>().join(" | ")));
        self.state.pending = planned;
        Ok(())
    }

    fn apply_source_override(&self, planned: &mut [Planned]) {
        for item in planned.iter_mut() {
            if !self.state.params.sources.is_empty() {
                item.sources = self.state.params.sources.clone();
            }
            item.sources.retain(|s| sources::ALL.contains(&s.as_str()));
            if item.sources.is_empty() {
                item.sources = vec![sources::ARXIV.into(), sources::GITHUB.into(), sources::WEB.into()];
            }
        }
    }

    // ----- search -----

    async fn search(&mut self) -> Result<(), RpcError> {
        if self.state.pending.is_empty() {
            return Ok(());
        }
        let pending = self.state.pending.clone();
        let total = pending.iter().map(|p| p.queries.len()).sum::<usize>() as u32;
        let mut done = 0;
        let mut offline: Option<String> = None;
        let mut found: Vec<(SearchHit, usize)> = Vec::new();
        for item in &pending {
            let index = self.question_index(&item.question);
            for query in &item.queries {
                self.check()?;
                done += 1;
                let params = SearchParams {
                    query: query.clone(),
                    sources: item.sources.clone(),
                    since: self.state.params.since.clone(),
                    limit: Some(self.config().deep_results_per_query),
                    sort: None,
                };
                match self.jobs.tools.search(params, self.cancel.clone()).await {
                    Ok(result) => {
                        let failed: Vec<String> = result.sources.iter().filter(|s| !s.ok).map(|s| format!("{} ({})", s.source, s.error.clone().unwrap_or_default())).collect();
                        let detail = if failed.is_empty() {
                            format!("\"{query}\": {} result(s)", result.results.len())
                        } else {
                            format!("\"{query}\": {} result(s); failed: {}", result.results.len(), failed.join(", "))
                        };
                        self.progress("search", done, total, detail);
                        for source in &result.sources {
                            if source.ok && !self.state.sources_searched.contains(&source.source) {
                                self.state.sources_searched.push(source.source.clone());
                            }
                        }
                        found.extend(result.results.into_iter().map(|hit| (hit, index)));
                    }
                    Err(error) if error.code == codes::CANCELLED => return Err(error),
                    Err(error) => {
                        if error.message.starts_with("offline") {
                            offline = Some(error.message.clone());
                        }
                        self.progress("search", done, total, format!("\"{query}\": {}", error.message));
                    }
                }
                self.state.queries_run.push(query.clone());
            }
        }
        if found.is_empty()
            && let Some(offline) = offline
        {
            return Err(RpcError::new(codes::UNAVAILABLE, offline));
        }
        let cited: HashSet<String> = self.topic_notes().await?.iter().map(|n| url_key(&n.url)).collect();
        let visited: HashSet<String> = self.state.visited.iter().cloned().collect();
        self.state.candidates = pick(found, &cited, &visited, self.state.params.max_sources.unwrap_or(self.config().deep_sources_per_round) as usize);
        self.state.pending.clear();
        self.progress("search", total, total, format!("{} source(s) to read", self.state.candidates.len()));
        Ok(())
    }

    fn question_index(&self, question: &str) -> usize {
        self.state.job.questions.iter().position(|q| q == question).unwrap_or_else(|| self.state.job.questions.len().saturating_sub(1))
    }

    // ----- read (map) -----

    async fn read(&mut self) -> Result<(), RpcError> {
        let candidates = self.state.candidates.clone();
        let total = candidates.len() as u32;
        let mut failure = None;
        for (index, candidate) in candidates.iter().enumerate() {
            self.check()?;
            let key = url_key(&candidate.read_url);
            if self.state.visited.contains(&key) {
                continue;
            }
            let position = index as u32 + 1;
            self.progress("read", position - 1, total, format!("reading {} ({})", candidate.hit.title, candidate.read_url));
            match self.read_one(candidate).await {
                Ok((added, dropped)) => {
                    self.state.visited.push(key.clone());
                    self.state.unread.retain(|source| url_key(&source.url) != key);
                    self.state.job.sources_read += 1;
                    self.state.dropped_quotes += dropped;
                    let dropped_text = if dropped > 0 { format!("; {dropped} dropped (quote not found in the source)") } else { String::new() };
                    self.progress("read", position, total, format!("{}: {added} note(s){dropped_text}", candidate.hit.title));
                }
                Err(ReadFailure::Extraction(error) | ReadFailure::Source(error)) if error.code == codes::CANCELLED => return Err(error),
                Err(failed) => {
                    let error = match failed {
                        ReadFailure::Source(error) => error,
                        ReadFailure::Extraction(error) => {
                            failure = Some(error.clone());
                            error
                        }
                    };
                    self.state.job.sources_failed += 1;
                    self.state.unread.retain(|source| url_key(&source.url) != key);
                    self.state.unread.push(Unread { title: candidate.hit.title.clone(), url: candidate.read_url.clone(), why: error.message.clone() });
                    self.progress("read", position, total, format!("{}: not read ({})", candidate.hit.title, error.message));
                }
            }
            self.state.job.notes = self.topic_notes().await?.len() as u32;
            self.checkpoint("read");
        }
        // Stay in read on extraction failure. Unavailable sources remain listed as unread,
        // while accessible sources can still support a report. Resume retries unvisited sources, even
        // when earlier chunks already added notes; extraction failure is not irrelevant text.
        if let Some(error) = failure { return Err(error); }
        Ok(())
    }

    async fn read_one(&self, candidate: &Candidate) -> Result<(u32, u32), ReadFailure> {
        let budget = self.config().chunk_chars * u64::from(self.config().max_chunks_per_source);
        let mut fetched = self.jobs.tools.fetch(FetchParams { url: candidate.read_url.clone(), max_chars: Some(budget), ..FetchParams::default() }).await;
        // A paper's PDF that cannot be read: its landing page still has the abstract.
        if fetched.is_err() && candidate.read_url != candidate.hit.url {
            fetched = self.jobs.tools.fetch(FetchParams { url: candidate.hit.url.clone(), max_chars: Some(budget), ..FetchParams::default() }).await;
        }
        let fetched = fetched.map_err(ReadFailure::Source)?;
        self.check()?;
        if fetched.text.trim().chars().count() < 200 {
            return Err(ReadFailure::Source(RpcError::new(codes::NOT_FOUND, "no readable text")));
        }
        let chunks = chunk(&fetched.text, self.config().chunk_chars as usize, self.config().max_chunks_per_source as usize);
        let questions = self.questions_block();
        let title = if fetched.title.is_empty() { candidate.hit.title.clone() } else { fetched.title.clone() };
        let date = fetched.published.clone().or_else(|| candidate.hit.date.clone());
        let (mut added, mut dropped) = (0, 0);
        for (part, text) in chunks.iter().enumerate() {
            self.check()?;
            let user = format!(
                "Questions:\n{questions}\n\nSource: {title}\nURL: {}\nDate: {}\nPart {} of {}:\n<<<\n{text}\n>>>",
                fetched.url,
                date.as_deref().unwrap_or("unknown"),
                part + 1,
                chunks.len()
            );
            let answer = self.ask(MAP_SYSTEM, user, map_schema(self.state.job.questions.len())).await?;
            self.check()?;
            let notes = answer.get("notes").and_then(Value::as_array).ok_or_else(|| {
                RpcError::internal(format!("the model returned no notes list for {} (part {})", fetched.url, part + 1))
            })?;
            let form = match_form(text);
            for item in notes {
                let claim = item.get("claim").and_then(Value::as_str).map(squash).unwrap_or_default();
                let quote = item.get("quote").and_then(Value::as_str).map(|q| q.trim().to_owned()).unwrap_or_default();
                if claim.is_empty() || !quote_in(&quote, &form) {
                    dropped += 1;
                    continue;
                }
                let question = item
                    .get("question")
                    .and_then(Value::as_u64)
                    .and_then(|n| self.state.job.questions.get((n as usize).saturating_sub(1)))
                    .or_else(|| candidate.questions.first().and_then(|i| self.state.job.questions.get(*i)))
                    .cloned();
                self.check()?;
                self.jobs
                    .notes
                    .add(NoteAddParams {
                        project_id: String::new(),
                        topic: self.state.job.topic.clone(),
                        claim,
                        quote,
                        url: fetched.final_url.clone().unwrap_or_else(|| fetched.url.clone()),
                        title: title.clone(),
                        date: date.clone(),
                        source: Some(candidate.hit.source.clone()),
                        question,
                        by: Some(self.state.job.job_id.clone()),
                    })
                    .await?;
                added += 1;
            }
        }
        Ok((added, dropped))
    }

    // ----- synthesize (reduce) -----

    async fn synthesize(&mut self) -> Result<(), RpcError> {
        let notes = self.topic_notes().await?;
        self.state.job.notes = notes.len() as u32;
        if notes.is_empty() {
            let summary = if self.state.job.sources_read == 0 && self.state.unread.is_empty() {
                "No readable source text was obtained."
            } else if self.state.job.sources_read == 0 {
                "No readable source text was obtained. Sources that could not be read are listed below."
            } else if self.state.unread.is_empty() {
                "No source that was read had relevant text that could be quoted."
            } else {
                "The sources that were read had no relevant text that could be quoted. Some sources could not be read and are listed below."
            };
            self.progress("synthesize", 0, 0, summary);
            self.state.synthesis = Some(Synthesis {
                summary: summary.into(),
                findings: Vec::new(),
                open_questions: self.state.job.questions.clone(),
            });
            return Ok(());
        }
        // Items to reduce: (text, note ids). Notes first; condensed findings when they do not fit.
        let mut items: Vec<(String, Vec<String>)> = notes
            .iter()
            .map(|n| (format!("({}, {}) {} — \"{}\"", n.title, n.date.as_deref().unwrap_or("undated"), n.claim, crate::text::clip(&n.quote, 400)), vec![n.id.clone()]))
            .collect();
        let budget = self.config().synth_chars as usize;
        let mut level = 0;
        while items.iter().map(|(t, _)| t.len() + 8).sum::<usize>() > budget && level < 3 {
            level += 1;
            let batches = batch(&items, budget);
            let total = batches.len() as u32;
            let mut condensed = Vec::new();
            for (i, group) in batches.iter().enumerate() {
                self.check()?;
                self.progress("synthesize", i as u32, total, format!("condensing notes, batch {} of {total}", i + 1));
                let synthesis = self.reduce_call(group, true).await?;
                for finding in synthesis.findings {
                    condensed.push((finding.statement, finding.notes));
                }
            }
            if condensed.is_empty() {
                break;
            }
            items = condensed;
        }
        self.progress("synthesize", 0, 1, format!("synthesizing {} note(s)", notes.len()));
        let mut synthesis = self.reduce_call(&items, false).await?;
        // Confidence: `high` needs two distinct sources among the cited notes.
        let url_of: HashMap<&str, &str> = notes.iter().map(|n| (n.id.as_str(), n.url.as_str())).collect();
        for finding in &mut synthesis.findings {
            let distinct: HashSet<&str> = finding.notes.iter().filter_map(|id| url_of.get(id.as_str()).copied()).collect();
            if finding.confidence == "high" && distinct.len() < 2 {
                finding.confidence = "medium".into();
            }
        }
        self.progress("synthesize", 1, 1, format!("{} finding(s), {} open question(s)", synthesis.findings.len(), synthesis.open_questions.len()));
        self.state.synthesis = Some(synthesis);
        Ok(())
    }

    /// One reduce call over labelled items; findings cite item labels, mapped back to note ids.
    async fn reduce_call(&self, items: &[(String, Vec<String>)], partial: bool) -> Result<Synthesis, RpcError> {
        let listing = items.iter().enumerate().map(|(i, (text, _))| format!("[{}] {text}", i + 1)).collect::<Vec<_>>().join("\n");
        let user = format!(
            "Topic: {}\nQuestions:\n{}\n\n{}:\n{listing}\n\n{}",
            self.state.job.topic,
            self.questions_block(),
            if partial { "Notes (one batch of several)" } else { "Notes" },
            if partial {
                "Condense this batch into findings; cite the note numbers each finding rests on."
            } else {
                "Write the summary and the findings; cite the note numbers each finding rests on; list what stays open."
            }
        );
        let answer = self.ask(REDUCE_SYSTEM, user, reduce_schema(items.len())).await?;
        Ok(synthesis_from(&answer, items))
    }

    // ----- gap check -----

    async fn gaps(&mut self) -> Result<bool, RpcError> {
        let Some(synthesis) = self.state.synthesis.clone() else { return Ok(false) };
        self.progress("gap", 0, 1, "checking what is still unanswered");
        let findings = synthesis.findings.iter().map(|f| format!("- ({}) {}", f.confidence, f.statement)).collect::<Vec<_>>().join("\n");
        let user = format!(
            "Topic: {}\nQuestions:\n{}\n\nSummary so far:\n{}\n\nFindings:\n{findings}\n\nOpen questions:\n{}\n\nQueries already run: {}\n\n{SOURCE_GUIDE}",
            self.state.job.topic,
            self.questions_block(),
            synthesis.summary,
            synthesis.open_questions.iter().map(|q| format!("- {q}")).collect::<Vec<_>>().join("\n"),
            self.state.queries_run.join("; "),
        );
        let answer = self.ask(GAP_SYSTEM, user, gap_schema()).await?;
        let done = answer.get("done").and_then(Value::as_bool).unwrap_or(true);
        let mut gaps = answer.get("gaps").map(|g| planned_from(&json!({ "questions": g }))).unwrap_or_default();
        let ran: HashSet<String> = self.state.queries_run.iter().map(|q| q.to_lowercase()).collect();
        for gap in &mut gaps {
            gap.queries.retain(|q| !ran.contains(&q.to_lowercase()));
        }
        gaps.retain(|g| !g.queries.is_empty());
        if done || gaps.is_empty() {
            self.progress("gap", 1, 1, "no gaps worth another round");
            return Ok(false);
        }
        self.apply_source_override(&mut gaps);
        for gap in &gaps {
            if !self.state.job.questions.contains(&gap.question) {
                self.state.job.questions.push(gap.question.clone());
            }
        }
        self.progress("gap", 1, 1, format!("another round for: {}", gaps.iter().map(|g| g.question.as_str()).collect::<Vec<_>>().join(" | ")));
        self.state.pending = gaps;
        Ok(true)
    }

    // ----- report -----

    async fn report(&mut self) -> Result<(), RpcError> {
        self.progress("report", 0, 1, "writing the report");
        let root = match self.state.params.root.clone().filter(|r| !r.trim().is_empty()) {
            Some(root) => root,
            None => self.jobs.tools.project_root(&self.state.job.project_id).await?,
        };
        let notes = self.topic_notes().await?;
        let text = crate::report::render(&self.state, &notes, &chrono::Utc::now().date_naive().to_string());
        let path = {
            let running = self.jobs.running.lock().map_err(|_| RpcError::internal("research job ownership unavailable"))?;
            self.check()?;
            if !running.get(&self.state.job.job_id).is_some_and(|worker| worker.generation == self.generation) {
                return Err(RpcError::cancelled());
            }
            let dir = PathBuf::from(&root).join("docs").join("research");
            std::fs::create_dir_all(&dir).map_err(|e| RpcError::internal(format!("{}: {e}", dir.display())))?;
            let path = dir.join(format!("{}.md", crate::text::slug(&self.state.job.topic)));
            std::fs::write(&path, text).map_err(|e| RpcError::internal(format!("{}: {e}", path.display())))?;
            path.to_string_lossy().into_owned()
        };
        self.state.job.report_path = Some(path.clone());
        let proposed = self.propose_knowledge(&notes).await;
        self.progress("done", 1, 1, if proposed > 0 { format!("report written: {path}; {proposed} knowledge item(s) proposed") } else { format!("report written: {path}") });
        Ok(())
    }

    /// What the job found, into the project's knowledge (docs/PROJECT-KNOWLEDGE-PLAN.md P5): every
    /// finding of high or medium confidence as a `fact` (source `research`, confidence 0.7 or 0.5,
    /// `SUPPORTED_BY` the notes it cites, the report as evidence) and every open question as a
    /// `question`. Low-confidence findings stay in the report only. Memory refusing or being away
    /// costs the proposal, never the job; returns how many were written.
    async fn propose_knowledge(&self, notes: &[Note]) -> u32 {
        let Some(synthesis) = self.state.synthesis.clone() else { return 0 };
        let project_id = self.state.job.project_id.trim();
        if project_id.is_empty() {
            return 0;
        }
        let note_ids: HashSet<&str> = notes.iter().map(|n| n.id.as_str()).collect();
        let report = self.state.job.report_path.clone().map(|p| format!("file://{p}"));
        let name_of = |text: &str| -> String {
            let mut name = String::new();
            for word in text.split_whitespace() {
                if !name.is_empty() && name.len() + 1 + word.len() > 60 {
                    break;
                }
                if !name.is_empty() {
                    name.push(' ');
                }
                name.push_str(word);
            }
            name
        };
        let mut written = 0;
        for finding in synthesis.findings.iter().filter(|f| matches!(f.confidence.as_str(), "high" | "medium")) {
            if self.check().is_err() { return written; }
            let statement = finding.statement.trim();
            if statement.is_empty() {
                continue;
            }
            let cited: Vec<&str> = finding.notes.iter().map(String::as_str).filter(|id| note_ids.contains(id)).collect();
            let mut evidence: Vec<String> = cited.iter().map(|id| format!("note://{id}")).collect();
            evidence.extend(report.iter().cloned());
            let params = cc_protocol::contracts::memory::KnowledgeUpsertParams {
                project_id: Some(project_id.to_owned()),
                kind: "fact".into(),
                name: name_of(statement),
                text: statement.to_owned(),
                state: None,
                source: Some("research".into()),
                confidence: Some(if finding.confidence == "high" { 0.7 } else { 0.5 }),
                as_of: None,
                by: self.state.job.job_id.clone(),
                evidence,
                aliases: Vec::new(),
                links: cited.iter().map(|id| cc_protocol::contracts::memory::KnowledgeLink { rel: "supported_by".into(), target: (*id).to_owned(), ..Default::default() }).collect(),
            };
            match self.jobs.notes.knowledge_upsert(params).await {
                Ok(_) => written += 1,
                Err(error) => {
                    tracing::warn!(%error, job = %self.state.job.job_id, "research finding not written to knowledge");
                    return written;
                }
            }
        }
        for question in synthesis.open_questions.iter().map(|q| q.trim()).filter(|q| !q.is_empty()) {
            if self.check().is_err() { return written; }
            let params = cc_protocol::contracts::memory::KnowledgeUpsertParams {
                project_id: Some(project_id.to_owned()),
                kind: "question".into(),
                name: name_of(question),
                text: question.to_owned(),
                state: None,
                source: Some("research".into()),
                confidence: Some(0.5),
                as_of: None,
                by: self.state.job.job_id.clone(),
                evidence: report.iter().cloned().collect(),
                aliases: Vec::new(),
                links: Vec::new(),
            };
            match self.jobs.notes.knowledge_upsert(params).await {
                Ok(_) => written += 1,
                Err(error) => {
                    tracing::warn!(%error, job = %self.state.job.job_id, "research question not written to knowledge");
                    return written;
                }
            }
        }
        written
    }
}

// ----- prompts and schemas -----

const PLAN_SYSTEM: &str = "You plan research. Break the topic into sub-questions that can be answered from papers, code repositories, discussions and web pages, and write the searches for each: short keyword queries as typed into a search engine (no quotes unless an exact phrase matters). Answer with JSON only.";

const SOURCE_GUIDE: &str = "Sources: arxiv (papers), semantic_scholar (papers with citations), github (repositories), github_issues (issues and pull requests), hackernews (discussion of tools and launches), reddit (practitioners' experience), web (blogs, documentation, articles).";

const MAP_SYSTEM: &str = "You extract evidence for research questions from one part of one source. Use only the text between <<< and >>>. Each note has: the number of the question it answers, a claim in your own words, and a quote copied verbatim from the text (one or two whole sentences, unchanged) that supports the claim. Do not add facts that are not in the text. If the part has nothing relevant, return an empty list. Answer with JSON only.";

const REDUCE_SYSTEM: &str = "You synthesize research notes. Use only the numbered notes given; never add facts from memory. Each finding states one conclusion and cites the numbers of the notes it rests on. Confidence: high when several independent sources agree, medium when one source supports it or sources partly disagree, low when evidence is thin or indirect. Mention disagreements between sources. Answer with JSON only.";

const GAP_SYSTEM: &str = "You review research coverage. Decide whether the questions are answered well enough. If not, name at most three follow-up questions that new searches could answer, each with new keyword queries (not the ones already run) and sources. Answer with JSON only.";

fn source_enum() -> Value {
    json!(sources::ALL)
}

pub fn plan_schema(max: usize) -> Value {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array", "minItems": 1, "maxItems": max,
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string"},
                        "queries": {"type": "array", "minItems": 1, "maxItems": 3, "items": {"type": "string"}},
                        "sources": {"type": "array", "minItems": 1, "maxItems": 4, "items": {"type": "string", "enum": source_enum()}}
                    },
                    "required": ["question", "queries", "sources"]
                }
            }
        },
        "required": ["questions"]
    })
}

pub fn map_schema(questions: usize) -> Value {
    json!({
        "type": "object",
        "properties": {
            "notes": {
                "type": "array", "maxItems": 6,
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "integer", "minimum": 1, "maximum": questions.max(1)},
                        "claim": {"type": "string"},
                        "quote": {"type": "string"}
                    },
                    "required": ["question", "claim", "quote"]
                }
            }
        },
        "required": ["notes"]
    })
}

pub fn reduce_schema(items: usize) -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "findings": {
                "type": "array", "maxItems": 12,
                "items": {
                    "type": "object",
                    "properties": {
                        "statement": {"type": "string"},
                        "notes": {"type": "array", "minItems": 1, "items": {"type": "integer", "minimum": 1, "maximum": items.max(1)}},
                        "confidence": {"type": "string", "enum": ["high", "medium", "low"]}
                    },
                    "required": ["statement", "notes", "confidence"]
                }
            },
            "open_questions": {"type": "array", "maxItems": 6, "items": {"type": "string"}}
        },
        "required": ["summary", "findings", "open_questions"]
    })
}

pub fn gap_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "done": {"type": "boolean"},
            "gaps": {
                "type": "array", "maxItems": 3,
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string"},
                        "queries": {"type": "array", "minItems": 1, "maxItems": 3, "items": {"type": "string"}},
                        "sources": {"type": "array", "minItems": 1, "maxItems": 4, "items": {"type": "string", "enum": source_enum()}}
                    },
                    "required": ["question", "queries", "sources"]
                }
            }
        },
        "required": ["done", "gaps"]
    })
}

/// The JSON object in a model answer (a fenced block or surrounding text tolerated).
pub fn parse_json_answer(content: &str) -> Option<Value> {
    let trimmed = content.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    serde_json::from_str(&trimmed[start..=end]).ok()
}

fn planned_from(answer: &Value) -> Vec<Planned> {
    answer
        .get("questions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let question = squash(item.get("question")?.as_str()?);
                    let strings = |key: &str| -> Vec<String> {
                        item.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(squash).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
                    };
                    let mut queries = strings("queries");
                    queries.dedup();
                    if question.is_empty() || queries.is_empty() {
                        return None;
                    }
                    Some(Planned { question, queries, sources: strings("sources") })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn synthesis_from(answer: &Value, items: &[(String, Vec<String>)]) -> Synthesis {
    let findings = answer
        .get("findings")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|f| {
                    let statement = squash(f.get("statement")?.as_str()?);
                    let mut notes: Vec<String> = Vec::new();
                    for label in f.get("notes").and_then(Value::as_array).cloned().unwrap_or_default() {
                        let Some(index) = label.as_u64().map(|n| n as usize).filter(|n| (1..=items.len()).contains(n)) else { continue };
                        for id in &items[index - 1].1 {
                            if !notes.contains(id) {
                                notes.push(id.clone());
                            }
                        }
                    }
                    // A finding citing nothing that exists is not grounded: dropped.
                    if statement.is_empty() || notes.is_empty() {
                        return None;
                    }
                    let confidence = match f.get("confidence").and_then(Value::as_str) {
                        Some(c @ ("high" | "medium" | "low")) => c.to_owned(),
                        _ => "low".to_owned(),
                    };
                    Some(Finding { statement, notes, confidence })
                })
                .collect()
        })
        .unwrap_or_default();
    Synthesis {
        summary: answer.get("summary").and_then(Value::as_str).map(|s| s.trim().to_owned()).unwrap_or_default(),
        findings,
        open_questions: answer.get("open_questions").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(squash).collect()).unwrap_or_default(),
    }
}

/// Rank hits: found by more queries first, then spread across sources; skip sources already cited
/// or visited. Papers are read from their PDF.
pub fn pick(found: Vec<(SearchHit, usize)>, cited: &HashSet<String>, visited: &HashSet<String>, max: usize) -> Vec<Candidate> {
    let mut by_key: Vec<(String, Candidate)> = Vec::new();
    for (hit, question) in found {
        let key = url_key(&hit.url);
        if let Some((_, existing)) = by_key.iter_mut().find(|(k, _)| *k == key) {
            existing.found_by += 1;
            if !existing.questions.contains(&question) {
                existing.questions.push(question);
            }
            continue;
        }
        let read_url = match hit.source.as_str() {
            sources::ARXIV | sources::SEMANTIC_SCHOLAR => hit.extra.get("pdf").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| hit.url.clone()),
            _ => hit.url.clone(),
        };
        if cited.contains(&key) || cited.contains(&url_key(&read_url)) || visited.contains(&key) || visited.contains(&url_key(&read_url)) {
            continue;
        }
        by_key.push((key, Candidate { hit, read_url, questions: vec![question], found_by: 1 }));
    }
    let mut candidates: Vec<Candidate> = by_key.into_iter().map(|(_, c)| c).collect();
    // Stable: equal counts keep search order.
    candidates.sort_by(|a, b| b.found_by.cmp(&a.found_by));
    // Spread across sources: round-robin over sources within the ranked list.
    let mut order: Vec<String> = Vec::new();
    for c in &candidates {
        if !order.contains(&c.hit.source) {
            order.push(c.hit.source.clone());
        }
    }
    let mut queues: Vec<std::collections::VecDeque<Candidate>> = order.iter().map(|s| candidates.iter().filter(|c| &c.hit.source == s).cloned().collect()).collect();
    let mut picked = Vec::new();
    while picked.len() < max && queues.iter().any(|q| !q.is_empty()) {
        for queue in queues.iter_mut() {
            if picked.len() >= max {
                break;
            }
            if let Some(c) = queue.pop_front() {
                picked.push(c);
            }
        }
    }
    picked
}

/// Split text at paragraph boundaries into chunks of at most `size` characters (a longer paragraph
/// is cut), at most `max` chunks.
pub fn chunk(text: &str, size: usize, max: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n") {
        let mut paragraph = paragraph.trim().to_owned();
        while paragraph.chars().count() > size {
            let (head, _) = crate::text::take_chars(&paragraph, size);
            let head = head.to_owned();
            paragraph = paragraph[head.len()..].to_owned();
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            chunks.push(head);
        }
        if current.chars().count() + paragraph.chars().count() + 2 > size && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if !paragraph.is_empty() {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(&paragraph);
        }
        if chunks.len() >= max {
            break;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks.truncate(max);
    chunks
}

/// Group items so each group's text stays within `budget` characters.
fn batch(items: &[(String, Vec<String>)], budget: usize) -> Vec<Vec<(String, Vec<String>)>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut size = 0;
    for item in items {
        if size + item.0.len() > budget && !current.is_empty() {
            groups.push(std::mem::take(&mut current));
            size = 0;
        }
        size += item.0.len() + 8;
        current.push(item.clone());
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}
