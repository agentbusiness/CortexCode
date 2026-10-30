//! The `schedule@1` service: the methods, the clock, the firings and the culling
//! (docs/SCHEDULES-PLAN.md).
//!
//! 1. Routines are memory's (`memory.routine.*`); this plugin keeps a copy for the clock, loaded on
//!    activation and written through on every change.
//! 2. The clock compares wall-clock time with each active routine's `next_ms` at least every 15 s
//!    (macOS's monotonic clock stops while the Mac sleeps). A due routine is claimed (written with
//!    its next time) before it fires; whatever was missed fires once.
//! 3. A firing wakes a session (`schedule.fired`, which the chat flow takes), hands the brief to an
//!    agent (`agents.handoff`) or runs a command (`tools.run`).
//! 4. Culling uses structural facts only: a run's final status in a journal, the watched tasks'
//!    states, deleted sessions and projects, failures in a row, `until` and `max_runs`.

use crate::data::Texts;
use crate::when::{self, Schedule};
use cc_protocol::contracts::agents::{HandoffParams, methods as agents_methods, topics as agents_topics};
use cc_protocol::contracts::frame::{ProjectDeleted, SessionCreateParams, SessionDeleted, SessionKind, kinds as entry_kinds, topics as frame_topics};
use cc_protocol::contracts::lifecycle::{Hello, ProbeCheck, ProbeReport};
use cc_protocol::contracts::memory::{AgentRecord, Routine, RoutineRun, Task, methods as memory_methods, topics as memory_topics};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::schedule::{AddParams, EditParams, Fired, IdParams, ListAnswer, ListParams, RoutineAnswer, methods, topics};
use cc_protocol::contracts::tools::{RunParams, RunResult, methods as tools_methods};
use cc_protocol::host::Host;
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError, parse_params, to_value};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// The longest the clock sleeps between looks at the time.
const LOOK_EVERY: Duration = Duration::from_secs(15);
/// Failed runs in a row that pause a routine.
const PAUSE_AFTER_FAILURES: u32 = 3;
/// A command run's longest time.
const COMMAND_TIMEOUT_MS: u64 = 600_000;
/// The global routines' home project folder, under the CortexCode home.
const ROUTINES_HOME: &str = "routines";

pub fn hello() -> Hello {
    let mut hello = Hello::new("cc-scheduler", env!("CARGO_PKG_VERSION"), "schedule", "schedule@1");
    let all = [methods::ADD, methods::EDIT, methods::PAUSE, methods::RESUME, methods::REMOVE, methods::RUN_NOW, methods::LIST, methods::GET, methods::RUNS];
    hello.provides = all.iter().map(|method| (*method).to_owned()).collect();
    // A session's shell manages routines through `cx schedule ...`.
    hello.commands = hello.provides.clone();
    // Run ends (journal status entries), agents and tasks finishing, deleted sessions and projects.
    hello.subscribes = vec![
        frame_topics::JOURNAL_PREFIX.to_owned(),
        agents_topics::AGENT_CHANGED.to_owned(),
        memory_topics::TASK_CHANGED.to_owned(),
        frame_topics::SESSION_DELETED.to_owned(),
        frame_topics::PROJECT_DELETED.to_owned(),
    ];
    hello
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

/// A run in progress: a handoff waits for its agent's task.
#[derive(Clone)]
struct Running {
    run: RoutineRun,
    task_id: Option<String>,
}

struct Inner {
    routines: Mutex<BTreeMap<String, Routine>>,
    loaded: AtomicBool,
    running: Mutex<HashMap<String, Running>>,
    /// The model run going on in each session (from its journal's status entries).
    session_runs: Mutex<HashMap<String, String>>,
    texts: Mutex<Option<Arc<Texts>>>,
    changed: Notify,
    tasks: TaskTracker,
    park: CancellationToken,
}

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// A brief or command in one spacing and case, for telling duplicates.
fn normal(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn first_line(text: &str, limit: usize) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    if line.chars().count() <= limit { line.to_owned() } else { format!("{}…", line.chars().take(limit).collect::<String>()) }
}

fn short_id() -> String {
    format!("r-{}", &uuid::Uuid::new_v4().simple().to_string()[..6])
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn schedule_of(routine: &Routine) -> Option<Schedule> {
    serde_json::from_value(routine.schedule.clone()).ok()
}

/// A task that will not run again.
fn task_finished(status: &str) -> bool {
    matches!(status, "done" | "cancelled")
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                routines: Mutex::new(BTreeMap::new()),
                loaded: AtomicBool::new(false),
                running: Mutex::new(HashMap::new()),
                session_runs: Mutex::new(HashMap::new()),
                texts: Mutex::new(None),
                changed: Notify::new(),
                tasks: TaskTracker::new(),
                park: CancellationToken::new(),
            }),
        }
    }

    fn texts(&self, ctx: &Ctx) -> Arc<Texts> {
        lock(&self.inner.texts).get_or_insert_with(|| Arc::new(Texts::new(ctx.env.live_dir.clone()))).clone()
    }

    fn text(&self, ctx: &Ctx, key: &str, vars: &[(&str, &str)]) -> String {
        self.texts(ctx).get(key, vars)
    }

    // ---- records ---------------------------------------------------------------------------------

    async fn load(&self, host: &Host) -> Result<(), RpcError> {
        let listed = host.call(memory_methods::ROUTINE_LIST, json!({})).await?;
        let routines: Vec<Routine> = serde_json::from_value(listed.get("routines").cloned().unwrap_or(Value::Null)).unwrap_or_default();
        let mut map = lock(&self.inner.routines);
        for routine in routines {
            map.insert(routine.id.clone(), routine);
        }
        self.inner.loaded.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn ensure_loaded(&self, host: &Host) -> Result<(), RpcError> {
        if self.inner.loaded.load(Ordering::SeqCst) { Ok(()) } else { self.load(host).await }
    }

    fn get(&self, id: &str) -> Option<Routine> {
        lock(&self.inner.routines).get(id).cloned()
    }

    async fn find(&self, host: &Host, id: &str) -> Result<Routine, RpcError> {
        self.ensure_loaded(host).await?;
        if let Some(routine) = self.get(id) {
            return Ok(routine);
        }
        let routine: Routine = host.get(memory_methods::ROUTINE_GET, json!({"id": id})).await.map_err(|_| RpcError::not_found(format!("no routine {id}; `cx schedule list` shows them")))?;
        lock(&self.inner.routines).insert(routine.id.clone(), routine.clone());
        Ok(routine)
    }

    /// Write a routine through to memory and tell the listeners.
    async fn save(&self, ctx: &Ctx, host: &Host, mut routine: Routine, change: &str) -> Result<Routine, RpcError> {
        routine.updated_ms = now_ms();
        let stored: Routine = host.get(memory_methods::ROUTINE_PUT, serde_json::to_value(&routine)?).await?;
        lock(&self.inner.routines).insert(stored.id.clone(), stored.clone());
        ctx.publish(topics::CHANGED, json!({"routine": stored, "change": change}));
        self.inner.changed.notify_one();
        Ok(stored)
    }

    async fn record_run(&self, host: &Host, run: &RoutineRun) {
        if let Err(error) = host.call(memory_methods::ROUTINE_RUN_PUT, serde_json::to_value(run).unwrap_or(Value::Null)).await {
            tracing::warn!(%error, routine = %run.routine_id, "could not record a run");
        }
    }

    /// End a routine (it stays as history until removed).
    async fn end(&self, ctx: &Ctx, host: &Host, mut routine: Routine, reason: &str) {
        if routine.state == "ended" {
            return;
        }
        routine.state = "ended".to_owned();
        routine.state_reason = reason.to_owned();
        routine.next_ms = None;
        if let Err(error) = self.save(ctx, host, routine, "ended").await {
            tracing::warn!(%error, "could not end a routine");
        }
    }

    async fn pause_with(&self, ctx: &Ctx, host: &Host, mut routine: Routine, reason: &str) {
        routine.state = "paused".to_owned();
        routine.state_reason = reason.to_owned();
        if let Err(error) = self.save(ctx, host, routine, "paused").await {
            tracing::warn!(%error, "could not pause a routine");
        }
    }

    // ---- answers ---------------------------------------------------------------------------------

    fn next_text(&self, ctx: &Ctx, next: &[u64]) -> String {
        if next.is_empty() {
            return self.text(ctx, "no_next", &[]);
        }
        let now = now_ms();
        next.iter().map(|ms| when::show(*ms, now)).collect::<Vec<_>>().join(", ")
    }

    /// The routine's next firings: its `next_ms`, then what the schedule gives after it.
    fn upcoming(routine: &Routine) -> Vec<u64> {
        let Some(first) = routine.next_ms.filter(|_| routine.state == "active") else { return Vec::new() };
        let mut out = vec![first];
        if let Some(schedule) = schedule_of(routine) {
            out.extend(when::upcoming(&schedule, first, 2));
        }
        out.truncate(3);
        if let Some(until) = routine.until_ms {
            out.retain(|ms| *ms <= until);
        }
        if let Some(max) = routine.max_runs {
            out.truncate(max.saturating_sub(routine.run_count) as usize);
        }
        out
    }

    fn answer(&self, ctx: &Ctx, routine: Routine, key: &str, note: Option<String>, existing: bool) -> RoutineAnswer {
        let next = Self::upcoming(&routine);
        let note = note.map(|note| format!(" ({note})")).unwrap_or_default();
        let next_text = self.next_text(ctx, &next);
        let text = self.text(
            ctx,
            key,
            &[("id", &routine.id), ("title", &routine.title), ("kind", &routine.kind), ("when", &routine.when), ("note", &note), ("next", &next_text)],
        );
        RoutineAnswer { routine, next, runs: Vec::new(), existing, text }
    }

    fn line(&self, ctx: &Ctx, routine: &Routine) -> String {
        let next = Self::upcoming(routine);
        let next_text = if routine.state == "active" { self.next_text(ctx, &next[..next.len().min(1)]) } else { routine.state_reason.clone() };
        self.text(
            ctx,
            "line",
            &[("id", &routine.id), ("state", &routine.state), ("when", &routine.when), ("kind", &routine.kind), ("title", &routine.title), ("next", &next_text)],
        )
    }

    // ---- methods ---------------------------------------------------------------------------------

    async fn add(&self, ctx: &Ctx, params: AddParams) -> Result<RoutineAnswer, RpcError> {
        let host = Host::new(ctx.clone());
        self.ensure_loaded(&host).await?;
        let now = now_ms();
        let read = when::read(&params.when, now).map_err(RpcError::invalid_params)?;
        let kind = params.kind.clone().unwrap_or_else(|| if params.command.is_some() { "command".into() } else { "wake".into() });
        if !matches!(kind.as_str(), "wake" | "handoff" | "command") {
            return Err(RpcError::invalid_params(format!("kind `{kind}`: use wake, handoff or command")));
        }
        let session = match &params.session_id {
            Some(id) if !id.is_empty() => host.session_get(id).await.ok(),
            _ => None,
        };
        let scope = match (&params.scope, &session) {
            (Some(scope), _) if !scope.is_empty() => scope.clone(),
            (_, Some(session)) => session.project_id.clone(),
            _ => "global".to_owned(),
        };
        let brief = params.brief.clone().unwrap_or_default().trim().to_owned();
        let command = params.command.clone().unwrap_or_default().trim().to_owned();
        match kind.as_str() {
            "command" if command.is_empty() => return Err(RpcError::invalid_params("a command routine needs the command: --command \"cargo audit\"")),
            "wake" if session.is_none() => return Err(RpcError::invalid_params("a wake goes to a session: run it inside one, or use --kind handoff")),
            "wake" | "handoff" if brief.is_empty() && !params.manage => return Err(RpcError::invalid_params("say what to do: the brief is what the run reads")),
            _ => {}
        }
        let report_to = session.as_ref().map(|session| session.id.clone());
        let mut watches = params.watches.clone();
        let mut until_ms = None;
        match params.until.as_deref().map(str::trim) {
            Some("done") => {
                if watches.is_empty()
                    && let Some(lead) = &report_to
                {
                    watches = self.open_work_of(&host, &scope, lead).await;
                }
            }
            Some(text) if !text.is_empty() => {
                until_ms = Some(when::until(text, now).ok_or_else(|| RpcError::invalid_params(format!("until `{text}`: write done or a date (2026-12-01, 2026-12-01 18:00)")))?);
            }
            _ => {}
        }
        // The same thing scheduled again: the routine that has it, its watches merged.
        let key = normal(if kind == "command" { &command } else { &brief });
        let duplicate = lock(&self.inner.routines)
            .values()
            .find(|routine| {
                routine.state == "active"
                    && routine.scope == scope
                    && routine.kind == kind
                    && routine.report_to == report_to
                    && routine.manage == params.manage
                    && normal(if kind == "command" { &routine.command } else { &routine.brief }) == key
            })
            .cloned();
        if let Some(mut routine) = duplicate {
            let before = routine.watches.len();
            for task in watches {
                if !routine.watches.contains(&task) {
                    routine.watches.push(task);
                }
            }
            if routine.watches.len() != before {
                routine = self.save(ctx, &host, routine, "edited").await?;
            }
            return Ok(self.answer(ctx, routine, "existing", None, true));
        }
        let title = params.title.clone().filter(|title| !title.trim().is_empty()).unwrap_or_else(|| {
            if params.manage { "Check on the agents".to_owned() } else { first_line(if kind == "command" { &command } else { &brief }, 60) }
        });
        let created_run = params.run_id.clone().or_else(|| report_to.as_ref().and_then(|session| lock(&self.inner.session_runs).get(session).cloned()));
        let routine = Routine {
            id: short_id(),
            scope,
            title,
            kind,
            brief,
            command,
            when: params.when.trim().to_owned(),
            schedule: serde_json::to_value(&read.schedule)?,
            next_ms: when::next_after(&read.schedule, now),
            last_due_ms: None,
            state: "active".to_owned(),
            state_reason: String::new(),
            report_to: report_to.clone(),
            home_session: None,
            created_session: report_to,
            created_run,
            watches,
            manage: params.manage,
            until_ms,
            max_runs: params.max_runs.filter(|max| *max > 0),
            run_count: 0,
            fail_streak: 0,
            last_outcome: String::new(),
            last_summary: String::new(),
            last_ok_ms: None,
            mode: params.mode.clone().unwrap_or_else(|| "auto".to_owned()),
            flow: params.flow.clone(),
            by: params.by.clone().unwrap_or_else(|| "model".to_owned()),
            created_ms: now,
            updated_ms: now,
        };
        if routine.next_ms.is_none() {
            return Err(RpcError::invalid_params(format!("`{}` has no time in the future", routine.when)));
        }
        let routine = self.save(ctx, &host, routine, "added").await?;
        Ok(self.answer(ctx, routine, "added", read.note, false))
    }

    /// The unfinished tasks of the agents a session started (what `--until done` watches).
    async fn open_work_of(&self, host: &Host, project_id: &str, lead: &str) -> Vec<String> {
        let Ok(listed) = host.call(memory_methods::AGENT_LIST, json!({"project_id": project_id})).await else { return Vec::new() };
        let agents: Vec<AgentRecord> = serde_json::from_value(listed.get("agents").cloned().unwrap_or(Value::Null)).unwrap_or_default();
        let mut out = Vec::new();
        for agent in agents {
            let (Some(session), Some(task)) = (&agent.session_id, &agent.task_id) else { continue };
            if host.session_get(session).await.ok().and_then(|session| session.parent).as_deref() != Some(lead) {
                continue;
            }
            if let Ok(found) = host.get::<Task>(memory_methods::TASK_GET, json!({"id": task})).await
                && !task_finished(&found.status)
            {
                out.push(task.clone());
            }
        }
        out
    }

    async fn edit(&self, ctx: &Ctx, params: EditParams) -> Result<RoutineAnswer, RpcError> {
        let host = Host::new(ctx.clone());
        let mut routine = self.find(&host, &params.id).await?;
        let now = now_ms();
        let mut note = None;
        if let Some(written) = params.when.as_deref().filter(|text| !text.trim().is_empty()) {
            let read = when::read(written, now).map_err(RpcError::invalid_params)?;
            routine.when = written.trim().to_owned();
            routine.schedule = serde_json::to_value(&read.schedule)?;
            routine.next_ms = when::next_after(&read.schedule, now);
            note = read.note;
        }
        if let Some(title) = params.title.filter(|text| !text.trim().is_empty()) {
            routine.title = title.trim().to_owned();
        }
        if let Some(kind) = params.kind.filter(|text| !text.trim().is_empty()) {
            if !matches!(kind.as_str(), "wake" | "handoff" | "command") {
                return Err(RpcError::invalid_params(format!("kind `{kind}`: use wake, handoff or command")));
            }
            routine.kind = kind;
        }
        if let Some(brief) = params.brief {
            routine.brief = brief.trim().to_owned();
        }
        if let Some(command) = params.command {
            routine.command = command.trim().to_owned();
        }
        match params.until.as_deref().map(str::trim) {
            Some("") => routine.until_ms = None,
            Some("done") => {}
            Some(text) => routine.until_ms = Some(when::until(text, now).ok_or_else(|| RpcError::invalid_params(format!("until `{text}`: write a date (2026-12-01)")))?),
            None => {}
        }
        if let Some(max) = params.max_runs {
            routine.max_runs = (max > 0).then_some(max);
        }
        if routine.state == "ended" && routine.next_ms.is_some() {
            routine.state = "active".to_owned();
            routine.state_reason.clear();
        }
        let routine = self.save(ctx, &host, routine, "edited").await?;
        Ok(self.answer(ctx, routine, "edited", note, false))
    }

    async fn pause(&self, ctx: &Ctx, params: IdParams) -> Result<RoutineAnswer, RpcError> {
        let host = Host::new(ctx.clone());
        let mut routine = self.find(&host, &params.id).await?;
        routine.state = "paused".to_owned();
        routine.state_reason = "paused".to_owned();
        let routine = self.save(ctx, &host, routine, "paused").await?;
        Ok(self.answer(ctx, routine, "paused", None, false))
    }

    async fn resume(&self, ctx: &Ctx, params: IdParams) -> Result<RoutineAnswer, RpcError> {
        let host = Host::new(ctx.clone());
        let mut routine = self.find(&host, &params.id).await?;
        let now = now_ms();
        routine.state = "active".to_owned();
        routine.state_reason.clear();
        routine.fail_streak = 0;
        if routine.next_ms.is_none_or(|next| next < now)
            && let Some(schedule) = schedule_of(&routine)
        {
            routine.next_ms = when::next_after(&schedule, now);
        }
        if routine.next_ms.is_none() {
            return Err(RpcError::invalid_params(format!("`{}` has no time in the future: give it a new one with `cx schedule edit {} --when ...`", routine.when, routine.id)));
        }
        let routine = self.save(ctx, &host, routine, "resumed").await?;
        Ok(self.answer(ctx, routine, "resumed", None, false))
    }

    async fn remove(&self, ctx: &Ctx, params: IdParams) -> Result<RoutineAnswer, RpcError> {
        let host = Host::new(ctx.clone());
        let routine = self.find(&host, &params.id).await?;
        host.call(memory_methods::ROUTINE_REMOVE, json!({"id": routine.id})).await?;
        lock(&self.inner.routines).remove(&routine.id);
        ctx.publish(topics::CHANGED, json!({"routine": routine, "change": "removed"}));
        Ok(self.answer(ctx, routine, "removed", None, false))
    }

    async fn run_now(&self, ctx: &Ctx, params: IdParams) -> Result<RoutineAnswer, RpcError> {
        let host = Host::new(ctx.clone());
        let routine = self.find(&host, &params.id).await?;
        let this = self.clone();
        let (ctx2, fired) = (ctx.clone(), routine.clone());
        self.inner.tasks.spawn(async move { this.fire(&ctx2, fired, now_ms()).await });
        Ok(self.answer(ctx, routine, "run_now", None, false))
    }

    async fn list(&self, ctx: &Ctx, params: ListParams) -> Result<ListAnswer, RpcError> {
        let host = Host::new(ctx.clone());
        self.ensure_loaded(&host).await?;
        let routines: Vec<Routine> = lock(&self.inner.routines)
            .values()
            .filter(|routine| match &params.scope {
                None => true,
                Some(scope) => &routine.scope == scope || (params.with_global && routine.scope == "global"),
            })
            .cloned()
            .collect();
        let text = if routines.is_empty() {
            format!("{}\n", self.text(ctx, "none", &[]))
        } else {
            routines.iter().map(|routine| self.line(ctx, routine) + "\n").collect()
        };
        Ok(ListAnswer { routines, text })
    }

    async fn show(&self, ctx: &Ctx, params: IdParams) -> Result<RoutineAnswer, RpcError> {
        let host = Host::new(ctx.clone());
        let routine = self.find(&host, &params.id).await?;
        let runs = self.runs_of(&host, &routine.id, 5).await;
        let mut text = self.line(ctx, &routine) + "\n";
        let what = if routine.kind == "command" { &routine.command } else { &routine.brief };
        if !what.is_empty() {
            text.push_str(what);
            text.push('\n');
        }
        let now = now_ms();
        for run in &runs {
            let summary = first_line(&run.summary, 160);
            text.push_str(&format!("- {}  {}  {summary}\n", when::show(run.started_ms, now), run.outcome));
        }
        let mut answer = self.answer(ctx, routine, "none", None, false);
        answer.text = text;
        answer.runs = runs;
        Ok(answer)
    }

    async fn runs_of(&self, host: &Host, id: &str, limit: usize) -> Vec<RoutineRun> {
        host.call(memory_methods::ROUTINE_RUNS, json!({"id": id, "limit": limit}))
            .await
            .ok()
            .and_then(|value| serde_json::from_value(value.get("runs").cloned().unwrap_or(Value::Null)).ok())
            .unwrap_or_default()
    }

    // ---- the clock -------------------------------------------------------------------------------

    fn start_clock(&self, ctx: &Ctx) {
        let this = self.clone();
        let ctx = ctx.clone();
        self.inner.tasks.spawn(async move {
            loop {
                let host = Host::new(ctx.clone());
                if let Err(error) = this.ensure_loaded(&host).await {
                    tracing::debug!(%error, "routines not loaded yet");
                }
                let now = now_ms();
                let due: Vec<Routine> = lock(&this.inner.routines)
                    .values()
                    .filter(|routine| routine.state == "active" && routine.next_ms.is_some_and(|next| next <= now))
                    .cloned()
                    .collect();
                for routine in due {
                    this.claim_and_fire(&ctx, routine, now).await;
                }
                let soonest = lock(&this.inner.routines).values().filter(|routine| routine.state == "active").filter_map(|routine| routine.next_ms).min();
                let wait = soonest.map(|next| Duration::from_millis(next.saturating_sub(now_ms()).max(200))).unwrap_or(LOOK_EVERY).min(LOOK_EVERY);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = this.inner.changed.notified() => {}
                    _ = this.inner.park.cancelled() => return,
                }
            }
        });
    }

    /// Claim a due routine (its next time written first, so it fires once) and fire it.
    async fn claim_and_fire(&self, ctx: &Ctx, mut routine: Routine, now: u64) {
        let host = Host::new(ctx.clone());
        let due = routine.next_ms.unwrap_or(now);
        // Whatever was missed fires once; the next time counts from now.
        routine.last_due_ms = Some(due);
        routine.run_count += 1;
        routine.next_ms = schedule_of(&routine).and_then(|schedule| when::next_after(&schedule, now));
        let exhausted = routine.max_runs.is_some_and(|max| routine.run_count >= max)
            || routine.next_ms.is_none()
            || routine.until_ms.is_some_and(|until| routine.next_ms.is_some_and(|next| next > until));
        if exhausted {
            routine.state = "ended".to_owned();
            routine.state_reason = if routine.next_ms.is_none() { "ran its last time".to_owned() } else { "reached its end".to_owned() };
            routine.next_ms = None;
        }
        let routine = match self.save(ctx, &host, routine, "fired").await {
            Ok(routine) => routine,
            Err(error) => {
                tracing::warn!(%error, "could not claim a due routine; it fires at the next look");
                return;
            }
        };
        let this = self.clone();
        let ctx = ctx.clone();
        self.inner.tasks.spawn(async move { this.fire(&ctx, routine, due).await });
    }

    async fn fire(&self, ctx: &Ctx, routine: Routine, due: u64) {
        let host = Host::new(ctx.clone());
        let started = now_ms();
        let mut run = RoutineRun {
            id: format!("{}:{}", routine.id, due),
            routine_id: routine.id.clone(),
            due_ms: due,
            started_ms: started,
            ended_ms: None,
            outcome: "running".to_owned(),
            summary: String::new(),
            session_id: None,
            task_id: None,
        };
        if lock(&self.inner.running).contains_key(&routine.id) {
            run.outcome = "skipped".to_owned();
            run.summary = "the last run still runs".to_owned();
            run.ended_ms = Some(started);
            self.record_run(&host, &run).await;
            return;
        }
        match routine.kind.as_str() {
            "wake" => self.fire_wake(ctx, &host, routine, run).await,
            "handoff" => self.fire_handoff(ctx, &host, routine, run).await,
            "command" => self.fire_command(ctx, &host, routine, run).await,
            other => tracing::warn!(kind = other, "a routine of an unknown kind"),
        }
    }

    async fn fire_wake(&self, ctx: &Ctx, host: &Host, routine: Routine, mut run: RoutineRun) {
        let Some(session_id) = routine.report_to.clone() else {
            self.end(ctx, host, routine, "it has no session to wake").await;
            return;
        };
        if host.session_get(&session_id).await.is_err() {
            self.end(ctx, host, routine, "its session is gone").await;
            return;
        }
        let project_id = if routine.scope == "global" { String::new() } else { routine.scope.clone() };
        let fired = Fired {
            id: run.id.clone(),
            routine_id: routine.id.clone(),
            session_id: session_id.clone(),
            project_id,
            title: routine.title.clone(),
            when: routine.when.clone(),
            text: routine.brief.clone(),
            manage: routine.manage,
        };
        ctx.publish(topics::FIRED, serde_json::to_value(&fired).unwrap_or(Value::Null));
        run.outcome = "woke".to_owned();
        run.session_id = Some(session_id);
        run.ended_ms = Some(now_ms());
        self.record_run(host, &run).await;
        // Wake runs leave the routine's outcome alone except the time.
        let mut routine = self.get(&routine.id).unwrap_or(routine);
        routine.last_outcome = "woke".to_owned();
        let _ = self.save(ctx, host, routine, "ran").await;
    }

    /// The project a routine's runs work in: its own, or (global) the Routines home project.
    async fn run_project(&self, ctx: &Ctx, host: &Host, routine: &Routine) -> Result<String, RpcError> {
        if routine.scope != "global" {
            return Ok(routine.scope.clone());
        }
        let root = ctx.env.home.join(ROUTINES_HOME);
        std::fs::create_dir_all(&root).map_err(|error| RpcError::internal(format!("{}: {error}", root.display())))?;
        let project = host.call(cc_protocol::contracts::frame::methods::PROJECT_OPEN, json!({"root": root})).await?;
        Ok(str_field(&project, "id").to_owned())
    }

    /// The routine's home session (made again when it is gone): its runs are this session's children.
    async fn home_session(&self, ctx: &Ctx, host: &Host, routine: &mut Routine, project_id: &str) -> Result<String, RpcError> {
        if let Some(id) = &routine.home_session
            && host.session_get(id).await.is_ok()
        {
            return Ok(id.clone());
        }
        let session = host
            .session_create(&SessionCreateParams {
                project_id: project_id.to_owned(),
                kind: SessionKind::Chat,
                title: format!("Routine: {}", routine.title),
                agent: None,
                parent: None,
                workspace: None,
                task_id: None,
                flow: routine.flow.clone(),
            })
            .await?;
        routine.home_session = Some(session.id.clone());
        *routine = self.save(ctx, host, routine.clone(), "edited").await?;
        Ok(session.id)
    }

    async fn fire_handoff(&self, ctx: &Ctx, host: &Host, routine: Routine, mut run: RoutineRun) {
        let mut routine = self.get(&routine.id).unwrap_or(routine);
        let started = async {
            let project_id = self.run_project(ctx, host, &routine).await?;
            let home = self.home_session(ctx, host, &mut routine, &project_id).await?;
            let mut brief = routine.brief.clone();
            if let Some(last_ok) = routine.last_ok_ms
                && !routine.last_summary.trim().is_empty()
            {
                brief.push_str(&self.text(ctx, "since", &[("date", &when::show(last_ok, now_ms())), ("summary", routine.last_summary.trim())]));
            }
            let params = HandoffParams { from_session: home, project: Some(project_id), flow: routine.flow.clone(), brief, title: Some(routine.title.clone()), mode: routine.mode.clone() };
            host.get::<AgentRecord>(agents_methods::HANDOFF, serde_json::to_value(&params)?).await
        }
        .await;
        match started {
            Ok(agent) => {
                run.session_id = agent.session_id.clone();
                run.task_id = agent.task_id.clone();
                lock(&self.inner.running).insert(routine.id.clone(), Running { run: run.clone(), task_id: agent.task_id.clone() });
                self.record_run(host, &run).await;
            }
            Err(error) => {
                run.summary = error.message.clone();
                self.finish(ctx, host, &routine.id, run, false).await;
            }
        }
    }

    async fn fire_command(&self, ctx: &Ctx, host: &Host, routine: Routine, mut run: RoutineRun) {
        let mut routine = self.get(&routine.id).unwrap_or(routine);
        lock(&self.inner.running).insert(routine.id.clone(), Running { run: run.clone(), task_id: None });
        self.record_run(host, &run).await;
        let outcome = async {
            let project_id = self.run_project(ctx, host, &routine).await?;
            let root = host.project_get(&project_id).await?.root;
            let home = self.home_session(ctx, host, &mut routine, &project_id).await?;
            let params = RunParams { workspace: root, command: routine.command.clone(), timeout_ms: Some(COMMAND_TIMEOUT_MS), background: false, session_id: Some(home.clone()), env: Value::Null };
            let result: RunResult = host.get(tools_methods::RUN, serde_json::to_value(&params)?).await?;
            Ok::<_, RpcError>((home, project_id, result))
        }
        .await;
        match outcome {
            Ok((home, project_id, result)) => {
                let ok = result.exit_code == Some(0) && !result.timed_out;
                run.session_id = Some(home.clone());
                run.summary = tail(result.output.trim(), 20);
                if !ok {
                    // The model hears only about a failure: the session that made it, else the home one.
                    let code = if result.timed_out { "after timing out".to_owned() } else { result.exit_code.map(|code| code.to_string()).unwrap_or_else(|| "without a code".to_owned()) };
                    let target = match &routine.report_to {
                        Some(session) if host.session_get(session).await.is_ok() => session.clone(),
                        _ => home,
                    };
                    let text = self.text(ctx, "command_failed", &[("command", &routine.command), ("code", &code), ("tail", &tail(result.output.trim(), 40))]);
                    let fired = Fired { id: run.id.clone(), routine_id: routine.id.clone(), session_id: target, project_id, title: routine.title.clone(), when: routine.when.clone(), text, manage: false };
                    ctx.publish(topics::FIRED, serde_json::to_value(&fired).unwrap_or(Value::Null));
                }
                self.finish(ctx, host, &routine.id, run, ok).await;
            }
            Err(error) => {
                run.summary = error.message.clone();
                self.finish(ctx, host, &routine.id, run, false).await;
            }
        }
    }

    /// A run ended: record it, update the routine (failures in a row pause it), tell the listeners.
    async fn finish(&self, ctx: &Ctx, host: &Host, routine_id: &str, mut run: RoutineRun, ok: bool) {
        lock(&self.inner.running).remove(routine_id);
        run.outcome = if ok { "done" } else { "failed" }.to_owned();
        run.ended_ms = Some(now_ms());
        self.record_run(host, &run).await;
        let Some(mut routine) = self.get(routine_id) else { return };
        routine.last_outcome = run.outcome.clone();
        routine.last_summary = run.summary.clone();
        if ok {
            routine.fail_streak = 0;
            routine.last_ok_ms = run.ended_ms;
        } else {
            routine.fail_streak += 1;
        }
        let pause = !ok && routine.fail_streak >= PAUSE_AFTER_FAILURES && routine.state == "active";
        if pause {
            routine.state = "paused".to_owned();
            routine.state_reason = format!("{} runs failed in a row", routine.fail_streak);
        }
        match self.save(ctx, host, routine, if pause { "paused" } else { "ran" }).await {
            Ok(routine) => ctx.publish(topics::RAN, json!({"routine": routine, "run": run})),
            Err(error) => tracing::warn!(%error, "could not record a run's end"),
        }
    }

    // ---- events: run ends and culling --------------------------------------------------------------

    /// A journal patch: a model run started (remember its id) or ended (cull).
    async fn journal_patch(&self, ctx: &Ctx, data: Value) {
        if str_field(&data, "op") != "create" || data.pointer("/entry/kind").and_then(Value::as_str) != Some(entry_kinds::STATUS) {
            return;
        }
        let session = data.pointer("/entry/session_id").and_then(Value::as_str).unwrap_or("").to_owned();
        let state = data.pointer("/entry/data/state").and_then(Value::as_str).unwrap_or("").to_owned();
        let run_id = data.pointer("/entry/data/run_id").and_then(Value::as_str).unwrap_or("").to_owned();
        if session.is_empty() {
            return;
        }
        if matches!(state.as_str(), "running" | "waiting" | "pending" | "") {
            if state == "running" && !run_id.is_empty() {
                lock(&self.inner.session_runs).insert(session, run_id);
            }
            return;
        }
        lock(&self.inner.session_runs).remove(&session);
        self.cull_run_end(ctx, &session, &run_id, matches!(state.as_str(), "stopped" | "cancelled")).await;
        self.cull_watched(ctx).await;
    }

    /// Rules 2 and 3: duplicates one run made end (the older one); a run the user stopped leaves its
    /// routines that have not fired yet paused.
    async fn cull_run_end(&self, ctx: &Ctx, session: &str, run_id: &str, stopped: bool) {
        if run_id.is_empty() {
            return;
        }
        let host = Host::new(ctx.clone());
        let made: Vec<Routine> = lock(&self.inner.routines)
            .values()
            .filter(|routine| routine.state == "active" && routine.created_session.as_deref() == Some(session) && routine.created_run.as_deref() == Some(run_id))
            .cloned()
            .collect();
        let mut seen: HashMap<(String, String, bool, String), String> = HashMap::new();
        let mut sorted = made.clone();
        sorted.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
        for routine in sorted {
            let key = (routine.scope.clone(), routine.kind.clone(), routine.manage, normal(if routine.kind == "command" { &routine.command } else { &routine.brief }));
            if let Some(newer) = seen.get(&key) {
                self.end(ctx, &host, routine, &format!("replaced by {newer} from the same run")).await;
                continue;
            }
            seen.insert(key, routine.id.clone());
            if stopped && routine.run_count == 0 {
                self.pause_with(ctx, &host, routine, "the run that made it was stopped").await;
            }
        }
    }

    /// Rule 1: a routine whose watched tasks are all finished ends.
    async fn cull_watched(&self, ctx: &Ctx) {
        let host = Host::new(ctx.clone());
        let watching: Vec<Routine> = lock(&self.inner.routines).values().filter(|routine| routine.state != "ended" && !routine.watches.is_empty()).cloned().collect();
        for routine in watching {
            let mut all_finished = true;
            for task in &routine.watches {
                match host.get::<Task>(memory_methods::TASK_GET, json!({"id": task})).await {
                    Ok(found) if !task_finished(&found.status) => {
                        all_finished = false;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) if error.code == cc_protocol::codes::NOT_FOUND => {}
                    Err(_) => {
                        all_finished = false;
                        break;
                    }
                }
            }
            if all_finished {
                self.end(ctx, &host, routine, "its work is finished").await;
            }
        }
    }

    /// An agent changed: a handoff run whose agent finished ends; watched work may be finished.
    async fn agent_changed(&self, ctx: &Ctx, data: Value) {
        let status = str_field(&data, "status").to_owned();
        let task_id = str_field(&data, "task_id").to_owned();
        if !task_id.is_empty() && matches!(status.as_str(), "done" | "blocked" | "stopped") {
            let found = lock(&self.inner.running).iter().find(|(_, running)| running.task_id.as_deref() == Some(task_id.as_str())).map(|(id, running)| (id.clone(), running.clone()));
            if let Some((routine_id, running)) = found {
                let host = Host::new(ctx.clone());
                let mut run = running.run;
                run.summary = host.get::<Task>(memory_methods::TASK_GET, json!({"id": task_id})).await.ok().and_then(|task| task.result).unwrap_or_default();
                self.finish(ctx, &host, &routine_id, run, status == "done").await;
            }
        }
        if matches!(status.as_str(), "done" | "stopped") {
            self.cull_watched(ctx).await;
        }
    }

    /// Rule 4: a wake whose session is deleted ends; a deleted home session is made again later.
    async fn session_deleted(&self, ctx: &Ctx, deleted: SessionDeleted) {
        let host = Host::new(ctx.clone());
        let affected: Vec<Routine> = lock(&self.inner.routines)
            .values()
            .filter(|routine| routine.state != "ended" && (routine.report_to.as_deref() == Some(deleted.session_id.as_str()) || routine.home_session.as_deref() == Some(deleted.session_id.as_str())))
            .cloned()
            .collect();
        for mut routine in affected {
            if routine.kind == "wake" && routine.report_to.as_deref() == Some(deleted.session_id.as_str()) {
                self.end(ctx, &host, routine, "its session is gone").await;
                continue;
            }
            if routine.report_to.as_deref() == Some(deleted.session_id.as_str()) {
                routine.report_to = None;
            }
            if routine.home_session.as_deref() == Some(deleted.session_id.as_str()) {
                routine.home_session = None;
            }
            let _ = self.save(ctx, &host, routine, "edited").await;
        }
    }

    /// Rule 5: a deleted project's routines are removed.
    async fn project_deleted(&self, ctx: &Ctx, deleted: ProjectDeleted) {
        let host = Host::new(ctx.clone());
        let gone: Vec<Routine> = lock(&self.inner.routines).values().filter(|routine| routine.scope == deleted.project_id).cloned().collect();
        for routine in gone {
            let _ = host.call(memory_methods::ROUTINE_REMOVE, json!({"id": routine.id})).await;
            lock(&self.inner.routines).remove(&routine.id);
            ctx.publish(topics::CHANGED, json!({"routine": routine, "change": "removed"}));
        }
    }

    /// Runs that were going when a predecessor stopped: a handoff's agent may still work; it is
    /// adopted from the run records (outcome `running`) and finished by its agent's next change.
    async fn readopt(&self, host: &Host) {
        let routines: Vec<Routine> = lock(&self.inner.routines).values().filter(|routine| routine.kind == "handoff").cloned().collect();
        for routine in routines {
            if let Some(run) = self.runs_of(host, &routine.id, 1).await.into_iter().next()
                && run.outcome == "running"
                && run.task_id.is_some()
            {
                let task_id = run.task_id.clone();
                lock(&self.inner.running).insert(routine.id.clone(), Running { run, task_id });
            }
        }
    }
}

impl Plugin for Scheduler {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let this = self.clone();
        Box::pin(async move {
            let Call { method, params, .. } = call;
            match method.as_str() {
                methods::ADD => to_value(&this.add(&ctx, parse_params(params)?).await?),
                methods::EDIT => to_value(&this.edit(&ctx, parse_params(params)?).await?),
                methods::PAUSE => to_value(&this.pause(&ctx, parse_params(params)?).await?),
                methods::RESUME => to_value(&this.resume(&ctx, parse_params(params)?).await?),
                methods::REMOVE => to_value(&this.remove(&ctx, parse_params(params)?).await?),
                methods::RUN_NOW => to_value(&this.run_now(&ctx, parse_params(params)?).await?),
                methods::LIST => to_value(&this.list(&ctx, parse_params(params)?).await?),
                methods::GET => to_value(&this.show(&ctx, parse_params(params)?).await?),
                methods::RUNS => {
                    let id = str_field(&params, "id").to_owned();
                    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
                    let host = Host::new(ctx.clone());
                    to_value(&json!({"runs": this.runs_of(&host, &id, limit).await}))
                }
                other => Err(RpcError::unknown_method(other)),
            }
        })
    }

    fn probe(&self, _ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let now = now_ms();
            let checks = [("every 5m", true), ("every mon,thu at 09:00", true), ("cron 0 9 * * 1", true), ("in 20m", true), ("whenever", false)];
            let mut out = Vec::new();
            for (written, readable) in checks {
                let read = when::read(written, now);
                let ok = read.is_ok() == readable && read.as_ref().map(|read| when::next_after(&read.schedule, now).is_some()).unwrap_or(true);
                out.push(ProbeCheck { name: format!("reads `{written}`"), ok, detail: String::new(), ms: started.elapsed().as_millis() as u64 });
            }
            Ok(ProbeReport::from_checks(out))
        })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let this = self.clone();
        Box::pin(async move {
            this.start_clock(&ctx);
            let adopt = this.clone();
            tokio::spawn(async move {
                let host = Host::new(ctx.clone());
                if adopt.ensure_loaded(&host).await.is_ok() {
                    adopt.readopt(&host).await;
                }
            });
            Ok(())
        })
    }

    fn event(&self, ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        let this = self.clone();
        Box::pin(async move {
            if topic.starts_with(frame_topics::JOURNAL_PREFIX) {
                this.journal_patch(&ctx, data).await;
            } else if topic == agents_topics::AGENT_CHANGED {
                this.agent_changed(&ctx, data).await;
            } else if topic == memory_topics::TASK_CHANGED {
                let status = data.pointer("/task/status").and_then(Value::as_str).unwrap_or("");
                if task_finished(status) {
                    this.cull_watched(&ctx).await;
                }
            } else if topic == frame_topics::SESSION_DELETED {
                if let Ok(deleted) = serde_json::from_value::<SessionDeleted>(data) {
                    this.session_deleted(&ctx, deleted).await;
                }
            } else if topic == frame_topics::PROJECT_DELETED
                && let Ok(deleted) = serde_json::from_value::<ProjectDeleted>(data)
            {
                this.project_deleted(&ctx, deleted).await;
            }
        })
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        let inner = self.inner.clone();
        Box::pin(async move {
            inner.park.cancel();
            inner.tasks.close();
            let _ = tokio::time::timeout(Duration::from_secs(2), inner.tasks.wait()).await;
        })
    }
}
