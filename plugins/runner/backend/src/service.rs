//! The `runner@1` service: jobs, their sessions' journals, facts, beats and checkpoints
//! (docs/RUNNER.md). Builds, commands, landing, releases and the reads of the mirror are in
//! [`crate::work`].
//!
//! 1. A job is prepared (`runner.prepare`: the mirror, a worktree, a project) and started
//!    (`runner.start`: a session in runner mode, the plan put in as approved, `plan.implement`).
//! 2. The session's journal is followed from the frame's events (`journal.<session>`), exactly:
//!    every patch carries the journal's revision, and a gap makes the journal be read again
//!    ([`Runner::rebuild`]). [`crate::track::Track`] counts it.
//! 3. A task that ended done is committed (`vcs.commit`): a checkpoint.
//! 4. The facts go out as `runner.beat` every 20 s while a job runs and at once on a change of
//!    task, a checkpoint, a held replan or an approval question. Three beats without an answer:
//!    the run stops at the next checkpoint.
//! 5. Every call that changes a job names its attempt; a replaced attempt is refused (`conflict`).

use crate::data::Texts;
use crate::store::{self, Job, Repo};
use crate::track::{Change, Track};
use cc_protocol::contracts::chatflow::{PlanPut, PlanPutParams, PlanPutTask, PlanRefParams, ReplanAnswerParams, RunnerSetParams, verbs};
use cc_protocol::contracts::frame::{JournalPatch, JournalReadResult, Project, SessionCreateParams, SessionKind, flows, methods as frame_methods, topics as frame_topics};
use cc_protocol::contracts::lifecycle::{Hello, ProbeReport};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::runner::{
    ApprovalAnswer, BeatAnswer, Checkpoint, Facts, PrepareParams, Prepared, ReplanAnswer, RestartParams, StartParams, Started, SteerParams, Task, methods, topics,
};
use cc_protocol::contracts::vcs::{self, methods as vcs_methods};
use cc_protocol::host::Host;
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError, codes, parse_params, to_value};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Between two beats of a running job.
const BEAT_EVERY: Duration = Duration::from_secs(20);
/// Beats without any answer after which the run stops at the next checkpoint.
const MISSED_BEATS: u32 = 3;
/// How long a call waits for a run it stopped to end.
const STOP_WAIT: Duration = Duration::from_secs(60);
/// The flow a job's session belongs to.
const FLOW: &str = flows::CODING;

pub fn hello() -> Hello {
    let mut hello = Hello::new("cc-runner", env!("CARGO_PKG_VERSION"), "runner", "runner@1");
    let all = [
        methods::PREPARE,
        methods::START,
        methods::GET,
        methods::LIST,
        methods::STOP,
        methods::RESTART,
        methods::STEER,
        methods::REPLAN_ANSWER,
        methods::APPROVAL_ANSWER,
        methods::DIFF,
        methods::BUILD,
        methods::COMMAND,
        methods::LAND,
        methods::REMOVE,
        methods::BEAT_ANSWER,
        methods::READ,
        methods::SEARCH,
        methods::TREE,
        methods::RELEASE,
    ];
    hello.provides = all.iter().map(|method| (*method).to_owned()).collect();
    // The jobs' sessions' journals (and their agents'): the frame sends every journal's patches.
    hello.subscribes = vec![frame_topics::JOURNAL_PREFIX.to_owned()];
    hello
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

/// A job or repository id as a folder name: letters, digits, `.`, `_`, `-`, not starting with a dot.
pub(crate) fn name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 100 && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// What the worktree holds against the last checkpoint and against the base.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tree {
    /// Head, changed files and lines: the total is measured again only when this moves.
    key: String,
    pub files: u32,
    pub lines: u32,
    pub total_files: u32,
    pub total_lines: u32,
    /// Every file changed since the base.
    pub changed: Vec<String>,
    pub out_of_scope: Vec<String>,
    pub protected_changed: Vec<String>,
}

/// A job with what this process knows about its run.
#[derive(Default)]
pub(crate) struct Live {
    pub job: Job,
    pub track: Track,
    /// The journal revision `track` reflects.
    rev: u64,
    /// Patches that arrived while the journal is read again.
    queue: Option<Vec<JournalPatch>>,
    /// The sessions of agents the job's session started, each counted on its own.
    children: BTreeMap<String, Track>,
    tree: Tree,
    /// Beats published since the manager last answered one.
    missed: u32,
    stop_at_checkpoint: bool,
    /// What the runner itself could not do (a commit that failed).
    error: String,
    /// A build, a landing or a restart of this job that is going on.
    pub busy: Option<&'static str>,
    /// Tokens of the sessions before the newest one (earlier restarts).
    spent_tokens: u64,
}

pub(crate) struct Inner {
    pub jobs: Mutex<BTreeMap<String, Live>>,
    /// Session -> its job (a job's own session and its agents').
    sessions: Mutex<HashMap<String, String>>,
    pub repos: Mutex<HashMap<String, Repo>>,
    /// Repositories a release is running in.
    pub releasing: Mutex<HashSet<String>>,
    loaded: AtomicBool,
    beating: AtomicBool,
    texts: Mutex<Option<Arc<Texts>>>,
    park: CancellationToken,
}

#[derive(Clone)]
pub struct Runner {
    pub(crate) inner: Arc<Inner>,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

/// What the event handler found to do after it took a patch in.
enum Follow {
    Nothing,
    Rebuild(String),
    Changes(String, Vec<Change>),
}

/// Marks a job busy until dropped.
pub(crate) struct Busy {
    runner: Runner,
    job_id: String,
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(live) = lock(&self.runner.inner.jobs).get_mut(&self.job_id) {
            live.busy = None;
        }
    }
}

/// A file a task may change, from one of its `touches`: a path (a file or a folder), with what
/// names a symbol in it cut off (`file.rs::name`, `file.py:name`). None for a bare symbol.
fn scope_path(touch: &str) -> Option<String> {
    let touch = touch.trim().trim_start_matches("./");
    let touch = touch.split("::").next().unwrap_or(touch);
    let touch = touch.split(':').next().unwrap_or(touch);
    let touch = touch.trim_end_matches("/**").trim_end_matches("/*").trim_end_matches('/');
    (!touch.is_empty() && (touch.contains('/') || touch.contains('.'))).then(|| touch.to_owned())
}

fn under(path: &str, scope: &str) -> bool {
    path == scope || path.strip_prefix(scope).is_some_and(|rest| rest.starts_with('/'))
}

impl Runner {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                jobs: Mutex::new(BTreeMap::new()),
                sessions: Mutex::new(HashMap::new()),
                repos: Mutex::new(HashMap::new()),
                releasing: Mutex::new(HashSet::new()),
                loaded: AtomicBool::new(false),
                beating: AtomicBool::new(false),
                texts: Mutex::new(None),
                park: CancellationToken::new(),
            }),
        }
    }

    pub(crate) fn text(&self, ctx: &Ctx, key: &str, vars: &[(&str, &str)]) -> String {
        let texts = lock(&self.inner.texts).get_or_insert_with(|| Arc::new(Texts::new(ctx.env.live_dir.clone()))).clone();
        texts.get(key, vars)
    }

    // ---- folders ---------------------------------------------------------------------------------

    /// `<CORTEX_HOME>/work`: one folder per repository, `mirror.git` and one worktree per job in it.
    pub(crate) fn work_dir(ctx: &Ctx) -> PathBuf {
        ctx.env.home.join("work")
    }

    pub(crate) fn mirror_path(ctx: &Ctx, repo: &str) -> PathBuf {
        Self::work_dir(ctx).join(repo).join("mirror.git")
    }

    // ---- records ---------------------------------------------------------------------------------

    /// The jobs and repositories the graph keeps, read once; each job's journal is read again.
    async fn load(&self, ctx: &Ctx) -> Result<(), RpcError> {
        let host = Host::new(ctx.clone());
        let repos = store::repos(&host).await?;
        let jobs = store::jobs(&host).await?;
        {
            let mut known = lock(&self.inner.repos);
            for repo in repos {
                known.entry(repo.repo.clone()).or_insert(repo);
            }
        }
        let mut sessions = Vec::new();
        {
            let mut map = lock(&self.inner.jobs);
            for job in jobs {
                if map.contains_key(&job.job_id) {
                    continue;
                }
                if !job.session_id.is_empty() {
                    sessions.push((job.job_id.clone(), job.session_id.clone()));
                }
                map.insert(job.job_id.clone(), Live { job, ..Live::default() });
            }
        }
        self.inner.loaded.store(true, Ordering::SeqCst);
        for (job_id, session_id) in sessions {
            lock(&self.inner.sessions).insert(session_id, job_id.clone());
            self.rebuild(ctx, &job_id).await;
        }
        Ok(())
    }

    pub(crate) async fn ensure_loaded(&self, ctx: &Ctx) -> Result<(), RpcError> {
        if self.inner.loaded.load(Ordering::SeqCst) { Ok(()) } else { self.load(ctx).await }
    }

    /// Write the job through to the graph. `before`: the job as it was, when tasks or checkpoints
    /// may have gone.
    pub(crate) async fn persist(&self, ctx: &Ctx, job_id: &str, before: Option<&Job>) {
        let Some(job) = lock(&self.inner.jobs).get(job_id).map(|live| live.job.clone()) else { return };
        if let Err(error) = store::put_job(&Host::new(ctx.clone()), &job, before).await {
            tracing::warn!(%error, job = %job_id, "the job could not be written to the graph");
        }
    }

    pub(crate) fn job(&self, ctx: &Ctx, job_id: &str) -> Result<Job, RpcError> {
        lock(&self.inner.jobs).get(job_id).map(|live| live.job.clone()).ok_or_else(|| RpcError::not_found(self.text(ctx, "no_job", &[("job", job_id)])))
    }

    /// The job, when `attempt` is its current one; a replaced attempt is refused.
    pub(crate) fn current(&self, ctx: &Ctx, job_id: &str, attempt: u32) -> Result<Job, RpcError> {
        let job = self.job(ctx, job_id)?;
        if job.attempt != attempt {
            return Err(RpcError::new(codes::CONFLICT, self.text(ctx, "stale", &[("job", job_id), ("current", &job.attempt.to_string()), ("attempt", &attempt.to_string())])));
        }
        Ok(job)
    }

    /// Mark the job busy with `what` until the guard drops; refused while it is busy already.
    pub(crate) fn busy(&self, ctx: &Ctx, job_id: &str, what: &'static str) -> Result<Busy, RpcError> {
        let mut jobs = lock(&self.inner.jobs);
        let live = jobs.get_mut(job_id).ok_or_else(|| RpcError::not_found(self.text(ctx, "no_job", &[("job", job_id)])))?;
        if let Some(doing) = live.busy {
            return Err(RpcError::new(codes::BUSY, format!("job {job_id} is busy ({doing}); try again when it is done")).retryable());
        }
        live.busy = Some(what);
        Ok(Busy { runner: self.clone(), job_id: job_id.to_owned() })
    }

    pub(crate) fn running(&self, job_id: &str) -> bool {
        lock(&self.inner.jobs).get(job_id).is_some_and(|live| live.track.running())
    }

    /// Change the job in place.
    pub(crate) fn update(&self, job_id: &str, change: impl FnOnce(&mut Live)) {
        if let Some(live) = lock(&self.inner.jobs).get_mut(job_id) {
            change(live);
        }
    }

    // ---- the journal -----------------------------------------------------------------------------

    /// One patch of a session's journal, taken in at once (patches arrive in order); what follows
    /// from it is done after.
    fn patch(&self, session_id: &str, patch: JournalPatch) -> Follow {
        let Some(job_id) = lock(&self.inner.sessions).get(session_id).cloned() else { return Follow::Nothing };
        let mut jobs = lock(&self.inner.jobs);
        let Some(live) = jobs.get_mut(&job_id) else { return Follow::Nothing };
        if live.job.session_id != session_id {
            // An agent's session: counted loosely (its tokens, its last call, when it last moved).
            if let (Some(track), JournalPatch::Create { entry, .. }) = (live.children.get_mut(session_id), &patch) {
                track.entry(entry);
            }
            return Follow::Nothing;
        }
        if let Some(queue) = live.queue.as_mut() {
            queue.push(patch);
            return Follow::Nothing;
        }
        let rev = match &patch {
            JournalPatch::Create { rev, .. } | JournalPatch::Append { rev, .. } | JournalPatch::Set { rev, .. } => *rev,
        };
        if rev != 0 && rev <= live.rev {
            return Follow::Nothing;
        }
        if rev != live.rev + 1 {
            // A patch was missed: the journal is read again, and what arrives meanwhile waits.
            live.queue = Some(vec![patch]);
            return Follow::Rebuild(job_id);
        }
        live.rev = rev;
        let changes = Self::apply(&mut live.track, &patch);
        if changes.is_empty() { Follow::Nothing } else { Follow::Changes(job_id, changes) }
    }

    fn apply(track: &mut Track, patch: &JournalPatch) -> Vec<Change> {
        match patch {
            JournalPatch::Create { entry, .. } => track.entry(entry),
            JournalPatch::Set { seq, field, value, .. } => track.set(*seq, field, value),
            JournalPatch::Append { seq, field, text, .. } => {
                track.append(*seq, field, text);
                Vec::new()
            }
        }
    }

    /// Read the job's session's journal whole and count it again, then take in what arrived
    /// meanwhile. What the journal shows and nobody acted on is acted on ([`Self::settle`]).
    async fn rebuild(&self, ctx: &Ctx, job_id: &str) {
        let Some(session_id) = lock(&self.inner.jobs).get_mut(job_id).map(|live| {
            live.queue.get_or_insert_with(Vec::new);
            live.job.session_id.clone()
        }) else {
            return;
        };
        let host = Host::new(ctx.clone());
        let read: Result<JournalReadResult, RpcError> = host.get(frame_methods::JOURNAL_READ, json!({"session_id": session_id, "from": 1})).await;
        let spawned = {
            let mut jobs = lock(&self.inner.jobs);
            let Some(live) = jobs.get_mut(job_id) else { return };
            let queued = live.queue.take().unwrap_or_default();
            let read = match read {
                Ok(read) if live.job.session_id == session_id => read,
                Ok(_) => return,
                Err(error) => {
                    tracing::warn!(%error, job = %job_id, "the job's journal could not be read; counted again at the next beat");
                    return;
                }
            };
            let mut track = Track::default();
            for entry in &read.entries {
                track.entry(entry);
            }
            let mut rev = read.rev;
            for patch in queued {
                let patch_rev = match &patch {
                    JournalPatch::Create { rev, .. } | JournalPatch::Append { rev, .. } | JournalPatch::Set { rev, .. } => *rev,
                };
                if patch_rev == rev + 1 {
                    rev = patch_rev;
                    Self::apply(&mut track, &patch);
                }
            }
            let spawned = track.spawned.clone();
            live.track = track;
            live.rev = rev;
            spawned
        };
        for child in spawned {
            self.follow_child(ctx, job_id, &child).await;
        }
        self.settle(ctx, job_id).await;
    }

    /// An agent's session of the job: its journal is counted from now on, what it holds first.
    async fn follow_child(&self, ctx: &Ctx, job_id: &str, session_id: &str) {
        lock(&self.inner.sessions).insert(session_id.to_owned(), job_id.to_owned());
        let through = match lock(&self.inner.jobs).get_mut(job_id) {
            Some(live) => live.children.entry(session_id.to_owned()).or_default().through,
            None => return,
        };
        let host = Host::new(ctx.clone());
        let Ok(entries) = host.journal_read(session_id, through + 1).await else { return };
        if let Some(track) = lock(&self.inner.jobs).get_mut(job_id).and_then(|live| live.children.get_mut(session_id)) {
            for entry in &entries {
                track.entry(entry);
            }
        }
    }

    /// After the journal was read again: an approval that waits is answered or reported, a task
    /// that ended done without a checkpoint gets one (the plugin was not running when it ended).
    async fn settle(&self, ctx: &Ctx, job_id: &str) {
        let (approval, unsaved) = {
            let jobs = lock(&self.inner.jobs);
            let Some(live) = jobs.get(job_id) else { return };
            let done: Vec<u32> = live.track.steps_done.iter().filter(|(_, outcome)| outcome == "done").map(|(n, _)| *n).collect();
            let unsaved = done.last().copied().filter(|n| {
                let task = Self::manager_task(&live.job, &live.track, *n).n;
                !live.job.checkpoints.iter().any(|checkpoint| checkpoint.task == task)
            });
            (live.track.approval.as_ref().map(|approval| approval.seq), unsaved)
        };
        if let Some(n) = unsaved {
            self.checkpoint(ctx, job_id, n).await;
        }
        if let Some(seq) = approval {
            self.approval_asked(ctx, job_id, seq).await;
        }
        self.beat(ctx, job_id, false).await;
    }

    /// The manager's task that the session's plan task `n` is: the one with its title, else the
    /// one at its place (the chat flow may add a task of its own to a plan, and an accepted
    /// replan renumbers).
    fn manager_task(job: &Job, track: &Track, n: u32) -> Task {
        let planned = track.plan_tasks.iter().find(|task| task.n == n);
        let own = job.tasks.iter().skip(job.offset as usize);
        if let Some(planned) = planned
            && let Some(task) = own.clone().find(|task| same(&task.title, &planned.title))
        {
            return task.clone();
        }
        let at = own.clone().nth(n.saturating_sub(1) as usize).or_else(|| job.tasks.last());
        Task {
            n: at.map(|task| task.n).unwrap_or(job.offset + n),
            title: planned.map(|task| task.title.clone()).or_else(|| at.map(|task| task.title.clone())).unwrap_or_default(),
            request: at.map(|task| task.request.clone()).unwrap_or_default(),
            ..planned.map(crate::track::PlanTask::task).unwrap_or_default()
        }
    }

    /// What follows from the changes a patch made.
    async fn follow(&self, ctx: &Ctx, job_id: &str, changes: Vec<Change>) {
        let mut tell = false;
        for change in changes {
            match change {
                Change::TaskStarted(_) | Change::RunEnded | Change::Replan => tell = true,
                Change::TaskEnded { n, outcome } => {
                    if outcome == "done" {
                        self.checkpoint(ctx, job_id, n).await;
                    }
                    tell = true;
                }
                Change::Approval(0) => tell = true,
                Change::Approval(seq) => {
                    self.approval_asked(ctx, job_id, seq).await;
                    tell = true;
                }
                Change::Spawned(session) => self.follow_child(ctx, job_id, &session).await,
                Change::PlanChanged => self.plan_changed(ctx, job_id).await,
            }
        }
        if tell {
            self.beat(ctx, job_id, false).await;
        }
    }

    /// The session's plan was written again (an accepted replan): the job's tasks from the
    /// session's first on become the plan's, each keeping the request of the task it was (by
    /// title, else by place).
    async fn plan_changed(&self, ctx: &Ctx, job_id: &str) {
        let before = {
            let mut jobs = lock(&self.inner.jobs);
            let Some(live) = jobs.get_mut(job_id) else { return };
            let offset = live.job.offset as usize;
            let old: Vec<Task> = live.job.tasks.iter().skip(offset).cloned().collect();
            let planned = live.track.plan_tasks.clone();
            let unchanged = old.len() == planned.len() && old.iter().zip(&planned).all(|(old, new)| same(&old.title, &new.title));
            if unchanged || planned.is_empty() {
                return;
            }
            let before = live.job.clone();
            let first = live.job.tasks.get(offset.wrapping_sub(1)).map(|task| task.n).unwrap_or(0);
            let mut request = old.first().map(|task| task.request.clone()).unwrap_or_default();
            let mut tasks: Vec<Task> = live.job.tasks.iter().take(offset).cloned().collect();
            for (at, planned) in planned.iter().enumerate() {
                if let Some(was) = old.iter().find(|task| same(&task.title, &planned.title)).or_else(|| old.get(at)) {
                    request = was.request.clone();
                }
                let mut task = planned.task();
                task.n = first + at as u32 + 1;
                task.depends_on = planned.depends_on.iter().map(|d| first + d).filter(|d| *d < task.n).collect();
                task.request = request.clone();
                tasks.push(task);
            }
            live.job.tasks = tasks;
            before
        };
        self.persist(ctx, job_id, Some(&before)).await;
    }

    // ---- approvals -------------------------------------------------------------------------------

    /// The builder asks before a call: what the plan lists is approved by the host; anything
    /// else waits for the manager's answer (it is in the facts).
    async fn approval_asked(&self, ctx: &Ctx, job_id: &str, seq: u64) {
        let (session_id, listed) = {
            let jobs = lock(&self.inner.jobs);
            let Some(live) = jobs.get(job_id) else { return };
            let Some(approval) = live.track.approval.as_ref().filter(|approval| approval.seq == seq) else { return };
            let listed = !approval.classes.is_empty() && approval.classes.iter().all(|class| live.job.approve.iter().any(|allowed| allowed == class));
            (live.job.session_id.clone(), listed)
        };
        if !listed {
            return;
        }
        let host = Host::new(ctx.clone());
        if let Err(error) = host.effect(&flows::method(FLOW, verbs::APPROVE), json!({"session_id": session_id, "seq": seq, "approve": true})).await {
            tracing::warn!(%error, job = %job_id, "an approval the plan lists could not be given");
        }
    }

    // ---- checkpoints -----------------------------------------------------------------------------

    /// The session's plan task `n` ended done: what it changed is committed by the host, with the
    /// task's title, its request and the job's trailers. A task that changed nothing is recorded
    /// at the commit that is there.
    async fn checkpoint(&self, ctx: &Ctx, job_id: &str, n: u32) {
        let Some((job, task)) = lock(&self.inner.jobs).get(job_id).map(|live| (live.job.clone(), Self::manager_task(&live.job, &live.track, n))) else { return };
        let host = Host::new(ctx.clone());
        let status: vcs::Status = match host.get(vcs_methods::STATUS, json!({"root": job.root})).await {
            Ok(status) => status,
            Err(error) => return self.failed(job_id, format!("the checkpoint of task {} could not be made: {}", task.n, error.message)),
        };
        let files = status.files.len() as u32;
        let lines = status.added_lines + status.removed_lines;
        let commit = if files == 0 {
            status.head.clone().unwrap_or_default()
        } else {
            let mut message = format!("{}\n", task.title.trim());
            let mut trailers: Vec<String> = Vec::new();
            if !task.request.trim().is_empty() {
                trailers.push(format!("Request: {}", task.request.trim()));
            }
            trailers.extend(job.trailers.iter().map(|line| line.trim().to_owned()).filter(|line| !line.is_empty()));
            if !trailers.is_empty() {
                message.push_str(&format!("\n{}\n", trailers.join("\n")));
            }
            match host.effect(vcs_methods::COMMIT, json!({"root": job.root, "message": message, "all": true})).await.and_then(parse_params::<vcs::CommitResult>) {
                Ok(result) => result.sha,
                Err(error) => return self.failed(job_id, format!("the checkpoint of task {} could not be committed: {}", task.n, error.message)),
            }
        };
        let checkpoint = Checkpoint { task: task.n, commit, ms: now_ms(), files, lines };
        let stop = {
            let mut jobs = lock(&self.inner.jobs);
            let Some(live) = jobs.get_mut(job_id) else { return };
            live.job.checkpoints.retain(|known| known.task != checkpoint.task);
            live.job.checkpoints.push(checkpoint);
            live.tree = Tree::default();
            live.error.clear();
            live.stop_at_checkpoint && live.track.running()
        };
        self.persist(ctx, job_id, None).await;
        if stop {
            // Three beats went unanswered: nobody watches this run.
            tracing::warn!(job = %job_id, "no answer to three beats: the run stops at this checkpoint");
            self.cancel_run(ctx, job_id).await;
        }
    }

    fn failed(&self, job_id: &str, message: String) {
        tracing::warn!(job = %job_id, "{message}");
        self.update(job_id, |live| live.error = message);
    }

    // ---- facts and beats -------------------------------------------------------------------------

    /// Measure the worktree: against the last checkpoint (its HEAD) at every call, against the
    /// base only when the first moved.
    async fn measure(&self, ctx: &Ctx, job: &Job, tree: Tree) -> Tree {
        if job.root.is_empty() || !Path::new(&job.root).is_dir() {
            return Tree::default();
        }
        let host = Host::new(ctx.clone());
        let Ok(status) = host.get::<vcs::Status>(vcs_methods::STATUS, json!({"root": job.root})).await else { return tree };
        if !status.is_repo {
            return Tree::default();
        }
        let files = status.files.len() as u32;
        let lines = status.added_lines + status.removed_lines;
        let key = format!("{}:{}:{}:{}", status.head.as_deref().unwrap_or(""), files, lines, job.base);
        if key == tree.key {
            return tree;
        }
        let mut next = Tree { key, files, lines, ..Tree::default() };
        if job.base.is_empty() {
            return next;
        }
        match host.get::<vcs::Diff>(vcs_methods::DIFF, json!({"root": job.root, "scope": format!("since:{}", job.base)})).await {
            Ok(diff) => {
                next.total_files = diff.files.len() as u32;
                next.total_lines = diff.added + diff.removed;
                next.changed = diff.files.iter().map(|file| file.path.clone()).collect();
                let scopes: Vec<String> = job.tasks.iter().flat_map(|task| task.touches.iter()).filter_map(|touch| scope_path(touch)).collect();
                if !scopes.is_empty() {
                    next.out_of_scope = next.changed.iter().filter(|path| !scopes.iter().any(|scope| under(path, scope))).cloned().collect();
                }
                if !job.standard_change {
                    let protected: Vec<String> = job.protected.iter().filter_map(|path| scope_path(path).or_else(|| Some(path.trim().trim_end_matches('/').to_owned()).filter(|path| !path.is_empty()))).collect();
                    next.protected_changed = next.changed.iter().filter(|path| protected.iter().any(|scope| under(path, scope))).cloned().collect();
                }
            }
            Err(error) => {
                tracing::debug!(%error, job = %job.job_id, "the worktree could not be compared with the base");
                // Measured again at the next call.
                next.key.clear();
            }
        }
        next
    }

    /// The job's facts now.
    pub(crate) async fn facts(&self, ctx: &Ctx, job_id: &str) -> Result<Facts, RpcError> {
        let (job, tree) = {
            let jobs = lock(&self.inner.jobs);
            let live = jobs.get(job_id).ok_or_else(|| RpcError::not_found(self.text(ctx, "no_job", &[("job", job_id)])))?;
            (live.job.clone(), live.tree.clone())
        };
        let tree = self.measure(ctx, &job, tree).await;
        let mut jobs = lock(&self.inner.jobs);
        let live = jobs.get_mut(job_id).ok_or_else(|| RpcError::not_found(self.text(ctx, "no_job", &[("job", job_id)])))?;
        live.tree = tree.clone();
        let track = &live.track;
        let now = now_ms();
        let (state, because) = match track.state.as_str() {
            _ if live.job.session_id.is_empty() => ("waiting", ""),
            "" => ("waiting", ""),
            "running" if track.approval.is_some() => ("running", "approval"),
            "running" if track.replan.is_some() => ("running", "replan"),
            "running" => ("running", ""),
            "done" if track.plan_done() => ("done", "done"),
            "done" => ("failed", "error"),
            "cancelled" => ("stopped", "cancelled"),
            _ if track.context_full => ("failed", "context"),
            _ if matches!(track.error_code.as_str(), codes::UNAVAILABLE | codes::BUSY | codes::DRAINING | codes::CONNECTION_CLOSED | codes::TIMEOUT) => ("failed", "lane"),
            _ => ("failed", "error"),
        };
        let planned = track.plan_tasks.iter().find(|task| task.n == track.task);
        let checks: Vec<String> = planned.map(|task| task.checks.clone()).unwrap_or_default();
        let (same_failure, failing_command) = track.same_failure();
        let children_last = live.children.values().map(|child| child.last_ms).max().unwrap_or(0);
        let last_ms = track.last_ms.max(children_last);
        // The newest call, the session's own or one of its agents'.
        let newest = live.children.values().filter(|child| child.last_ms > track.last_ms).max_by_key(|child| child.last_ms).and_then(Track::last_call).or_else(|| track.last_call());
        let now_line = if state != "running" {
            String::new()
        } else if let Some(approval) = &track.approval {
            self.text(ctx, "now_approval", &[("tool", &approval.tool)])
        } else if track.replan.is_some() {
            self.text(ctx, "now_replan", &[])
        } else if track.stage == "wait" {
            self.text(ctx, "now_model", &[])
        } else {
            match newest {
                Some((tool, target)) => {
                    let target: String = target.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect();
                    let key = match tool {
                        "edit" | "write" => "now_edit",
                        "read" => "now_read",
                        "run" => "now_run",
                        "search" | "grep" | "find" => "now_search",
                        _ => "now_other",
                    };
                    self.text(ctx, key, &[("tool", tool), ("target", &target)]).trim().to_owned()
                }
                None => self.text(ctx, "now_plan", &[]),
            }
        };
        let ended = if state == "running" || state == "waiting" { now } else { last_ms.max(live.job.started_ms) };
        let error = match state {
            "failed" if !live.error.is_empty() => live.error.clone(),
            "failed" if !track.error_message.is_empty() => track.error_message.clone(),
            "failed" => track.detail.clone(),
            _ => live.error.clone(),
        };
        Ok(Facts {
            job_id: live.job.job_id.clone(),
            attempt: live.job.attempt,
            state: state.to_owned(),
            stopped_because: because.to_owned(),
            session_id: live.job.session_id.clone(),
            project_id: live.job.project_id.clone(),
            task: if track.task == 0 { 0 } else { Self::manager_task(&live.job, track, track.task).n },
            tasks: live.job.tasks.len() as u32,
            stage: track.stage.clone(),
            now: now_line,
            calls_since_edit: track.calls_since_edit(&checks),
            idle_ms: if state == "running" && last_ms > 0 { now.saturating_sub(last_ms) } else { 0 },
            same_call: track.same_call(),
            same_failure,
            failing_command,
            files: tree.files,
            lines: tree.lines,
            total_files: tree.total_files,
            total_lines: tree.total_lines,
            out_of_scope: tree.out_of_scope,
            protected_changed: tree.protected_changed,
            minutes: if live.job.started_ms == 0 { 0 } else { ended.saturating_sub(live.job.started_ms) / 60_000 },
            tokens: live.spent_tokens + track.tokens + live.children.values().map(|child| child.tokens).sum::<u64>(),
            checkpoints: live.job.checkpoints.clone(),
            replan: track.replan.clone(),
            approval: track.approval.clone(),
            error,
            ms: now,
        })
    }

    /// Every file changed since the job's base, as last measured ([`Self::facts`] measures).
    pub(crate) fn changed(&self, job_id: &str) -> Vec<String> {
        lock(&self.inner.jobs).get(job_id).map(|live| live.tree.changed.clone()).unwrap_or_default()
    }

    /// Publish the job's facts. `timed`: one of the regular beats, which the manager answers.
    async fn beat(&self, ctx: &Ctx, job_id: &str, timed: bool) {
        if timed {
            self.update(job_id, |live| {
                if live.missed >= MISSED_BEATS {
                    live.stop_at_checkpoint = true;
                }
                live.missed += 1;
            });
        }
        match self.facts(ctx, job_id).await {
            Ok(facts) => ctx.publish(topics::BEAT, serde_json::to_value(&facts).unwrap_or(Value::Null)),
            Err(error) => tracing::debug!(%error, job = %job_id, "no facts for a beat"),
        }
    }

    /// The clock: every 20 s, each running job's journal is checked against the frame's (a patch
    /// that never arrived shows as a revision that moved) and its facts go out.
    fn start_clock(&self, ctx: &Ctx) {
        if self.inner.beating.swap(true, Ordering::SeqCst) {
            return;
        }
        let (this, ctx, park) = (self.clone(), ctx.clone(), self.inner.park.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(BEAT_EVERY) => {}
                    _ = park.cancelled() => return,
                }
                let going: Vec<(String, String, u64, u64)> = lock(&this.inner.jobs)
                    .values()
                    .filter(|live| !live.job.session_id.is_empty() && (live.track.running() || live.track.state.is_empty()))
                    .map(|live| (live.job.job_id.clone(), live.job.session_id.clone(), live.track.through, live.rev))
                    .collect();
                for (job_id, session_id, through, rev) in going {
                    let host = Host::new(ctx.clone());
                    let read: Result<JournalReadResult, RpcError> = host.get(frame_methods::JOURNAL_READ, json!({"session_id": session_id, "from": through + 1})).await;
                    let rebuilding = lock(&this.inner.jobs).get(&job_id).is_none_or(|live| live.queue.is_some() || live.rev != rev);
                    if !rebuilding && read.is_ok_and(|read| read.rev != rev) {
                        this.rebuild(&ctx, &job_id).await;
                    }
                    this.beat(&ctx, &job_id, true).await;
                }
            }
        });
    }

    // ---- the chat flow ---------------------------------------------------------------------------

    async fn flow(&self, ctx: &Ctx, verb: &str, params: Value) -> Result<Value, RpcError> {
        Host::new(ctx.clone()).effect(&flows::method(FLOW, verb), params).await
    }

    /// Cancel the job's run (the chat flow acknowledges at once; the final status follows).
    pub(crate) async fn cancel_run(&self, ctx: &Ctx, job_id: &str) {
        let Some(session_id) = lock(&self.inner.jobs).get(job_id).map(|live| live.job.session_id.clone()).filter(|session| !session.is_empty()) else { return };
        if let Err(error) = self.flow(ctx, verbs::CANCEL, json!({"session_id": session_id})).await {
            tracing::warn!(%error, job = %job_id, "the run could not be cancelled");
        }
    }

    /// Wait until the job's run has ended (its final status arrived), at most `limit`.
    pub(crate) async fn wait_ended(&self, job_id: &str, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        while self.running(job_id) {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        true
    }

    /// A new session of the job's project in runner mode, the tasks put in as its approved plan,
    /// and `plan.implement` in mode `auto`. The session is followed from before its first entry.
    async fn start_session(&self, ctx: &Ctx, job_id: &str, tasks: &[Task]) -> Result<String, RpcError> {
        let job = self.job(ctx, job_id)?;
        let host = Host::new(ctx.clone());
        let session = host
            .session_create(&SessionCreateParams {
                project_id: job.project_id.clone(),
                kind: SessionKind::Chat,
                title: job.title.clone(),
                agent: None,
                parent: None,
                workspace: None,
                task_id: None,
                flow: Some(FLOW.to_owned()),
            })
            .await?;
        // The old session's tokens stay counted; the new one starts from nothing.
        let old = {
            let mut jobs = lock(&self.inner.jobs);
            let live = jobs.get_mut(job_id).ok_or_else(|| RpcError::not_found(self.text(ctx, "no_job", &[("job", job_id)])))?;
            let old = std::mem::replace(&mut live.job.session_id, session.id.clone());
            live.spent_tokens += live.track.tokens + live.children.values().map(|child| child.tokens).sum::<u64>();
            live.track = Track::default();
            live.rev = 0;
            live.queue = None;
            live.children.clear();
            live.missed = 0;
            live.stop_at_checkpoint = false;
            live.error.clear();
            live.job.stop_reason.clear();
            old
        };
        {
            let mut sessions = lock(&self.inner.sessions);
            sessions.retain(|_, job| job != job_id);
            sessions.insert(session.id.clone(), job_id.to_owned());
        }
        if !old.is_empty() {
            tracing::info!(job = %job_id, old = %old, new = %session.id, "the job goes on in a new session");
        }
        self.flow(ctx, verbs::RUNNER_SET, serde_json::to_value(RunnerSetParams { session_id: session.id.clone(), on: true })?).await?;
        // The manager's numbers become places in the list.
        let place: HashMap<u32, u32> = tasks.iter().enumerate().map(|(at, task)| (task.n, at as u32 + 1)).collect();
        let plan = PlanPut {
            goal: job.goal.clone(),
            approach: job.approach.clone(),
            tasks: tasks
                .iter()
                .map(|task| PlanPutTask {
                    title: task.title.clone(),
                    brief: task.brief.clone(),
                    known: task.known.clone(),
                    rules: task.rules.clone(),
                    depends_on: task.depends_on.iter().filter_map(|n| place.get(n).copied()).collect(),
                    touches: task.touches.clone(),
                    checks: task.checks.clone(),
                })
                .collect(),
        };
        let put = self.flow(ctx, verbs::PLAN_PUT, serde_json::to_value(PlanPutParams { session_id: session.id.clone(), plan })?).await?;
        let plan_id = str_field(&put, "plan_id").to_owned();
        if plan_id.is_empty() {
            return Err(RpcError::new(codes::CONTRACT_VIOLATION, "the chat flow answered plan.put without the plan's id"));
        }
        self.flow(ctx, verbs::PLAN_IMPLEMENT, serde_json::to_value(PlanRefParams { session_id: session.id.clone(), plan_id, mode: "auto".into() })?).await?;
        Ok(session.id)
    }

    // ---- methods ---------------------------------------------------------------------------------

    async fn prepare(&self, ctx: &Ctx, params: PrepareParams) -> Result<Prepared, RpcError> {
        self.ensure_loaded(ctx).await?;
        if !name_ok(&params.job_id) || !name_ok(&params.repo) {
            return Err(RpcError::invalid_params("`job_id` and `repo` are folder names: letters, digits, `.`, `_` and `-`"));
        }
        if params.job_id == "mirror.git" || params.job_id == "release" {
            return Err(RpcError::invalid_params(format!("`{}` is not a job's name", params.job_id)));
        }
        if params.main.trim().is_empty() || params.branch.trim().is_empty() || params.url.trim().is_empty() {
            return Err(RpcError::invalid_params("`url`, `main` and `branch` are needed"));
        }
        let existing = lock(&self.inner.jobs).get(&params.job_id).map(|live| (live.job.clone(), live.track.running()));
        if existing.as_ref().is_some_and(|(_, running)| *running) {
            return Err(RpcError::new(codes::CONFLICT, format!("job {} is running; stop it before it is prepared again", params.job_id)));
        }
        let host = Host::new(ctx.clone());
        let mirror = self.mirror(ctx, &params.repo, &params.url, &params.main).await?;
        let main_ref = format!("refs/remotes/origin/{}", params.main.trim());
        let kept = existing.as_ref().map(|(job, _)| job).filter(|job| job.branch == params.branch && !job.base.is_empty()).map(|job| job.base.clone());
        let wanted = if params.base.trim().is_empty() { kept.unwrap_or(main_ref) } else { params.base.trim().to_owned() };
        let base = host
            .get::<vcs::RevResult>(vcs_methods::REV, json!({"root": mirror, "rev": wanted}))
            .await?
            .sha
            .ok_or_else(|| RpcError::not_found(format!("the source has no commit `{wanted}` (branch `{}` of {})", params.main, params.url)))?;
        let root = Self::work_dir(ctx).join(&params.repo).join(&params.job_id);
        let tree: vcs::WorktreeResult = host
            .effect(vcs_methods::WORKTREE_ADD, json!({"repo": mirror, "path": root.to_string_lossy(), "branch": params.branch.trim(), "from": base}))
            .await
            .and_then(parse_params)?;
        // The tools keep their scratch files in `tmp/` and list it in `.gitignore` when git does not
        // ignore it yet. In a job that line would be a change no task asked for, so the worktree
        // ignores `tmp/` on its own, outside the tracked files.
        ignore_scratch(std::path::Path::new(&tree.path)).await;
        // Who the job's commits are made as: what the manager says for this repository.
        commit_as(std::path::Path::new(&tree.path), params.commit_name.trim(), params.commit_email.trim()).await?;
        let project: Project = host.effect(frame_methods::PROJECT_OPEN, json!({"root": tree.path})).await.and_then(parse_params)?;
        let before = existing.as_ref().map(|(job, _)| job.clone());
        {
            let mut jobs = lock(&self.inner.jobs);
            let live = jobs.entry(params.job_id.clone()).or_default();
            live.job.job_id = params.job_id.clone();
            live.job.repo = params.repo.clone();
            live.job.url = params.url.trim().to_owned();
            live.job.main = params.main.trim().to_owned();
            live.job.branch = params.branch.trim().to_owned();
            live.job.base = base.clone();
            live.job.root = tree.path.clone();
            live.job.mirror = mirror.clone();
            live.job.project_id = project.id.clone();
            live.tree = Tree::default();
        }
        self.persist(ctx, &params.job_id, before.as_ref()).await;
        Ok(Prepared { project_id: project.id, root: tree.path, base })
    }

    /// The repository's mirror, made when absent and fetched; its record is kept.
    pub(crate) async fn mirror(&self, ctx: &Ctx, repo: &str, url: &str, main: &str) -> Result<String, RpcError> {
        let host = Host::new(ctx.clone());
        let path = Self::mirror_path(ctx, repo);
        let made: vcs::MirrorResult = host.effect(vcs_methods::MIRROR, json!({"url": url.trim(), "path": path.to_string_lossy()})).await.and_then(parse_params)?;
        if !made.ok {
            return Err(RpcError::new(codes::UNAVAILABLE, format!("the source {url} could not be fetched: {}", made.detail)).retryable());
        }
        let record = Repo { repo: repo.to_owned(), url: url.trim().to_owned(), main: main.trim().to_owned(), mirror: made.path.clone() };
        let changed = lock(&self.inner.repos).insert(repo.to_owned(), record.clone()).as_ref() != Some(&record);
        if changed && let Err(error) = store::put_repo(&host, &record).await {
            tracing::warn!(%error, repo, "the repository could not be written to the graph");
        }
        Ok(made.path)
    }

    async fn start(&self, ctx: &Ctx, params: StartParams) -> Result<Started, RpcError> {
        self.ensure_loaded(ctx).await?;
        let job = self.job(ctx, &params.job_id)?;
        if params.attempt < job.attempt {
            return Err(RpcError::new(codes::CONFLICT, self.text(ctx, "stale", &[("job", &params.job_id), ("current", &job.attempt.to_string()), ("attempt", &params.attempt.to_string())])));
        }
        if !params.project_id.is_empty() && params.project_id != job.project_id {
            return Err(RpcError::invalid_params(format!("job {} was prepared as project {}, not {}", params.job_id, job.project_id, params.project_id)));
        }
        if params.tasks.is_empty() {
            return Err(RpcError::invalid_params("a job needs at least one task"));
        }
        let mut numbers: Vec<u32> = params.tasks.iter().map(|task| task.n).collect();
        numbers.sort_unstable();
        numbers.dedup();
        if numbers.len() != params.tasks.len() || numbers.first() == Some(&0) {
            return Err(RpcError::invalid_params("every task needs its own number `n`, from 1"));
        }
        let _busy = self.busy(ctx, &params.job_id, "starting")?;
        if self.running(&params.job_id) {
            return Err(RpcError::new(codes::CONFLICT, format!("job {} is running; stop it or restart a task", params.job_id)));
        }
        let before = job.clone();
        self.update(&params.job_id, |live| {
            live.job.attempt = params.attempt;
            live.job.title = params.title.trim().to_owned();
            live.job.goal = params.goal.trim().to_owned();
            live.job.approach = params.approach.trim().to_owned();
            live.job.tasks = params.tasks.clone();
            live.job.tasks.sort_by_key(|task| task.n);
            live.job.offset = 0;
            live.job.protected = params.protected.clone();
            live.job.standard_change = params.standard_change;
            live.job.approve = params.approve.clone();
            live.job.trailers = params.trailers.clone();
            live.job.started_ms = now_ms();
            live.job.squashed.clear();
            live.job.landed.clear();
            live.spent_tokens = 0;
        });
        let tasks = self.job(ctx, &params.job_id)?.tasks;
        let started = self.start_session(ctx, &params.job_id, &tasks).await;
        self.persist(ctx, &params.job_id, Some(&before)).await;
        let session_id = started?;
        self.beat(ctx, &params.job_id, false).await;
        Ok(Started { session_id })
    }

    async fn stop(&self, ctx: &Ctx, job_id: &str, attempt: u32) -> Result<Facts, RpcError> {
        self.ensure_loaded(ctx).await?;
        self.current(ctx, job_id, attempt)?;
        if self.running(job_id) {
            self.update(job_id, |live| live.job.stop_reason = "cancelled".into());
            self.cancel_run(ctx, job_id).await;
            self.wait_ended(job_id, Duration::from_secs(10)).await;
            self.persist(ctx, job_id, None).await;
        }
        self.facts(ctx, job_id).await
    }

    /// Back to the checkpoint before `task`, then the tasks from it on in a new session; the note
    /// is one more line of that task's brief.
    async fn restart(&self, ctx: &Ctx, params: RestartParams) -> Result<Started, RpcError> {
        self.ensure_loaded(ctx).await?;
        let job = self.current(ctx, &params.job_id, params.attempt)?;
        let Some(at) = job.tasks.iter().position(|task| task.n == params.task) else {
            return Err(RpcError::invalid_params(format!("job {} has no task {}", params.job_id, params.task)));
        };
        let _busy = self.busy(ctx, &params.job_id, "restarting a task")?;
        if self.running(&params.job_id) {
            self.cancel_run(ctx, &params.job_id).await;
            if !self.wait_ended(&params.job_id, STOP_WAIT).await {
                return Err(RpcError::new(codes::BUSY, format!("job {}'s run did not stop; try the restart again", params.job_id)).retryable());
            }
        }
        // The last checkpoint of a task before this one; none: the base.
        let earlier: Vec<u32> = job.tasks.iter().take(at).map(|task| task.n).collect();
        let back_to = job.checkpoints.iter().rev().find(|checkpoint| earlier.contains(&checkpoint.task) && !checkpoint.commit.is_empty()).map(|checkpoint| checkpoint.commit.clone()).unwrap_or_else(|| job.base.clone());
        let host = Host::new(ctx.clone());
        host.effect(vcs_methods::RESET, json!({"root": job.root, "to": back_to, "clean": true})).await?;
        let before = job.clone();
        let note = params.note.split_whitespace().collect::<Vec<_>>().join(" ");
        self.update(&params.job_id, |live| {
            live.job.checkpoints.retain(|checkpoint| earlier.contains(&checkpoint.task));
            live.job.offset = at as u32;
            live.job.squashed.clear();
            live.tree = Tree::default();
            if !note.is_empty()
                && let Some(task) = live.job.tasks.get_mut(at)
            {
                task.brief = format!("{}\n{note}", task.brief.trim_end());
            }
        });
        let tasks: Vec<Task> = self.job(ctx, &params.job_id)?.tasks.into_iter().skip(at).collect();
        let started = self.start_session(ctx, &params.job_id, &tasks).await;
        self.persist(ctx, &params.job_id, Some(&before)).await;
        let session_id = started?;
        self.beat(ctx, &params.job_id, false).await;
        Ok(Started { session_id })
    }

    /// A fixed sentence to the builder, at its next tool boundary. Never into a session that waits
    /// on an approval, and never into one that is not running (a message there starts a turn).
    async fn steer(&self, ctx: &Ctx, params: SteerParams) -> Result<Value, RpcError> {
        self.ensure_loaded(ctx).await?;
        let job = self.current(ctx, &params.job_id, params.attempt)?;
        if params.text.trim().is_empty() {
            return Err(RpcError::invalid_params("nothing to say"));
        }
        let (running, asking) = lock(&self.inner.jobs).get(&params.job_id).map(|live| (live.track.running(), live.track.approval.is_some())).unwrap_or((false, false));
        if asking {
            return Err(RpcError::new(codes::CONFLICT, self.text(ctx, "steer_approval", &[("job", &params.job_id)])));
        }
        if !running {
            return Err(RpcError::new(codes::CONFLICT, self.text(ctx, "not_running", &[("job", &params.job_id)])));
        }
        self.flow(ctx, verbs::STEER, json!({"session_id": job.session_id, "text": params.text.trim(), "kind": "steer"})).await?;
        Ok(json!({"ok": true}))
    }

    async fn replan_answer(&self, ctx: &Ctx, params: ReplanAnswer) -> Result<Value, RpcError> {
        self.ensure_loaded(ctx).await?;
        let job = self.current(ctx, &params.job_id, params.attempt)?;
        self.flow(ctx, verbs::REPLAN_ANSWER, serde_json::to_value(ReplanAnswerParams { session_id: job.session_id, accept: params.accept })?).await?;
        Ok(json!({"ok": true}))
    }

    /// The manager's answer to the builder's question; a refusal's sentence (what to do instead)
    /// reaches the builder as a message once the question is closed.
    async fn approval_answer(&self, ctx: &Ctx, params: ApprovalAnswer) -> Result<Value, RpcError> {
        self.ensure_loaded(ctx).await?;
        let job = self.current(ctx, &params.job_id, params.attempt)?;
        self.flow(ctx, verbs::APPROVE, json!({"session_id": job.session_id, "seq": params.seq, "approve": params.approve})).await?;
        if !params.approve && !params.text.trim().is_empty() && self.running(&params.job_id) {
            self.flow(ctx, verbs::STEER, json!({"session_id": job.session_id, "text": params.text.trim(), "kind": "steer"})).await?;
        }
        Ok(json!({"ok": true}))
    }

    async fn beat_answer(&self, ctx: &Ctx, params: BeatAnswer) -> Result<Value, RpcError> {
        self.ensure_loaded(ctx).await?;
        self.current(ctx, &params.job_id, params.attempt)?;
        self.update(&params.job_id, |live| {
            live.missed = 0;
            live.stop_at_checkpoint = false;
        });
        match params.want.as_str() {
            "stop" => {
                if self.running(&params.job_id) {
                    self.update(&params.job_id, |live| live.job.stop_reason = "cancelled".into());
                    self.cancel_run(ctx, &params.job_id).await;
                }
            }
            "continue" | "" => {}
            other => return Err(RpcError::invalid_params(format!("`want` is `continue` or `stop`, not `{other}`"))),
        }
        Ok(json!({"ok": true}))
    }

    async fn list(&self, ctx: &Ctx) -> Result<Value, RpcError> {
        self.ensure_loaded(ctx).await?;
        let ids: Vec<String> = lock(&self.inner.jobs).keys().cloned().collect();
        let mut jobs = Vec::new();
        for id in ids {
            if let Ok(facts) = self.facts(ctx, &id).await {
                jobs.push(facts);
            }
        }
        Ok(json!({"jobs": jobs}))
    }

    /// The worktree and the project go; the branch stays in the mirror.
    async fn remove(&self, ctx: &Ctx, job_id: &str) -> Result<Value, RpcError> {
        self.ensure_loaded(ctx).await?;
        let job = self.job(ctx, job_id)?;
        let _busy = self.busy(ctx, job_id, "being removed")?;
        if self.running(job_id) {
            self.cancel_run(ctx, job_id).await;
            if !self.wait_ended(job_id, STOP_WAIT).await {
                return Err(RpcError::new(codes::BUSY, format!("job {job_id}'s run did not stop; try the removal again")).retryable());
            }
        }
        let host = Host::new(ctx.clone());
        if !job.root.is_empty() && !job.mirror.is_empty() && Path::new(&job.mirror).is_dir() {
            host.effect(vcs_methods::WORKTREE_REMOVE, json!({"repo": job.mirror, "path": job.root, "force": true})).await?;
        }
        if !job.project_id.is_empty()
            && let Err(error) = host.effect(frame_methods::PROJECT_DELETE, json!({"id": job.project_id})).await
            && error.code != codes::NOT_FOUND
        {
            tracing::warn!(%error, job = %job_id, "the job's project could not be deleted");
        }
        if let Err(error) = store::retire_job(&host, &job).await {
            tracing::warn!(%error, job = %job_id, "the job could not be retired in the graph");
        }
        lock(&self.inner.sessions).retain(|_, job| job != job_id);
        lock(&self.inner.jobs).remove(job_id);
        Ok(json!({"ok": true}))
    }
}

impl Plugin for Runner {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let this = self.clone();
        Box::pin(async move {
            let Call { method, params, cancel, .. } = call;
            let job_of = |params: &Value| str_field(params, "job_id").to_owned();
            let attempt_of = |params: &Value| params.get("attempt").and_then(Value::as_u64).unwrap_or(0) as u32;
            match method.as_str() {
                methods::PREPARE => to_value(&this.prepare(&ctx, parse_params(params)?).await?),
                methods::START => to_value(&this.start(&ctx, parse_params(params)?).await?),
                methods::GET => {
                    this.ensure_loaded(&ctx).await?;
                    to_value(&this.facts(&ctx, &job_of(&params)).await?)
                }
                methods::LIST => this.list(&ctx).await,
                methods::STOP => to_value(&this.stop(&ctx, &job_of(&params), attempt_of(&params)).await?),
                methods::RESTART => to_value(&this.restart(&ctx, parse_params(params)?).await?),
                methods::STEER => this.steer(&ctx, parse_params(params)?).await,
                methods::REPLAN_ANSWER => this.replan_answer(&ctx, parse_params(params)?).await,
                methods::APPROVAL_ANSWER => this.approval_answer(&ctx, parse_params(params)?).await,
                methods::BEAT_ANSWER => this.beat_answer(&ctx, parse_params(params)?).await,
                methods::REMOVE => this.remove(&ctx, &job_of(&params)).await,
                methods::DIFF => to_value(&this.diff(&ctx, &params).await?),
                methods::BUILD => to_value(&this.build(&ctx, parse_params(params)?, &cancel).await?),
                methods::COMMAND => to_value(&this.command(&ctx, parse_params(params)?, &cancel).await?),
                methods::LAND => to_value(&this.land(&ctx, parse_params(params)?).await?),
                methods::READ => this.read(&ctx, parse_params(params)?).await,
                methods::SEARCH => this.search(&ctx, parse_params(params)?).await,
                methods::TREE => this.tree(&ctx, &params).await,
                methods::RELEASE => to_value(&this.release(&ctx, parse_params(params)?, &cancel).await?),
                other => Err(RpcError::unknown_method(other)),
            }
        })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        let this = self.clone();
        Box::pin(async move { Ok(this.probe_work(&ctx).await) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let this = self.clone();
        Box::pin(async move {
            this.start_clock(&ctx);
            // The jobs are in the graph, whose plugin may start later: read in the background.
            tokio::spawn(async move {
                if let Err(error) = this.ensure_loaded(&ctx).await {
                    tracing::warn!(%error, "the runner's jobs could not be read from the graph yet; read again at the next call");
                }
            });
            Ok(())
        })
    }

    fn event(&self, ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        // Taken in here, in arrival order; only what follows from it runs later.
        let follow = match topic.strip_prefix(frame_topics::JOURNAL_PREFIX) {
            Some(session_id) if lock(&self.inner.sessions).contains_key(session_id) => match serde_json::from_value::<JournalPatch>(data) {
                Ok(patch) => self.patch(session_id, patch),
                Err(_) => Follow::Nothing,
            },
            _ => Follow::Nothing,
        };
        let this = self.clone();
        Box::pin(async move {
            match follow {
                Follow::Nothing => {}
                Follow::Rebuild(job_id) => this.rebuild(&ctx, &job_id).await,
                Follow::Changes(job_id, changes) => this.follow(&ctx, &job_id, changes).await,
            }
        })
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        self.inner.park.cancel();
        Box::pin(async {})
    }
}

/// List `/tmp/` in the worktree's own exclude file (`info/exclude`), once.
async fn ignore_scratch(root: &std::path::Path) {
    let asked = tokio::process::Command::new("git").arg("-C").arg(root).args(["rev-parse", "--git-path", "info/exclude"]).output().await;
    let Ok(output) = asked else { return };
    let named = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || named.is_empty() {
        return;
    }
    let path = if std::path::Path::new(&named).is_absolute() { std::path::PathBuf::from(named) } else { root.join(named) };
    let text = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    if text.lines().any(|line| line.trim() == "/tmp/") {
        return;
    }
    if let Some(folder) = path.parent() {
        let _ = tokio::fs::create_dir_all(folder).await;
    }
    let separator = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
    if let Err(error) = tokio::fs::write(&path, format!("{text}{separator}/tmp/\n")).await {
        tracing::warn!(%error, "the worktree's exclude file could not be written; the tools will list tmp/ in .gitignore");
    }
}

/// Set the repository's own git identity (author and committer of every commit made in it) to the
/// one given; with none given, take back one set before, so git's own identity on this computer
/// is used. A job must not start with an identity nobody chose for it, so a failure stops it.
async fn commit_as(root: &std::path::Path, name: &str, email: &str) -> Result<(), RpcError> {
    let git = |args: Vec<String>| {
        let root = root.to_owned();
        async move { tokio::process::Command::new("git").arg("-C").arg(&root).args(&args).output().await }
    };
    for (key, value) in [("user.name", name), ("user.email", email)] {
        if name.is_empty() || email.is_empty() {
            // Not set is not an error.
            let _ = git(vec!["config".into(), "--unset".into(), key.into()]).await;
            continue;
        }
        let done = git(vec!["config".into(), key.into(), value.into()]).await;
        if !done.as_ref().is_ok_and(|output| output.status.success()) {
            return Err(RpcError::internal(format!("the commit identity ({key}) could not be set in {}", root.display())));
        }
    }
    Ok(())
}
