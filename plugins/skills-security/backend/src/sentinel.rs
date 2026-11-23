//! The security sentinel (`cc_protocol::contracts::security`): one loop that keeps every project of
//! this computer watched and decides what reaches the person.
//!
//! - **Cadence.** A delta sweep every `delta_minutes` (15) reads which advisories changed since the
//!   last one and re-checks only the packages they name; a full sweep every `full_hours` (24), and
//!   at the first start, reads every tree again. A request (`skills-security.sweep`) runs next.
//! - **A sweep** runs the secaudit tool once for every project (`fleet --json`), writes the
//!   findings into each project's graph, closes the ones a complete sweep no longer sees, and
//!   records the feed cursor, the trees' stamps and posture, and what it found on the computer.
//! - **The filter** then judges every finding not judged yet (`triage.rs`), wherever it came from:
//!   a sweep, an audit, a review. Findings the model could not settle are investigated, one session
//!   at a time.
//! - **Notifications.** An alert that is urgent (malicious, a campaign indicator, a live credential,
//!   exploited, critical) is told at once; the other alerts wait for one daily digest. Nothing
//!   quiet or muted is ever told.

use crate::store::{self, Saved};
use crate::triage::{self, Cards, Judgment};
use cc_protocol::contracts::memory::{SecurityFinding, SecurityTriage, topics as memory_topics};
use cc_protocol::contracts::notify::{Notification, kinds, methods as notify_methods};
use cc_protocol::contracts::security::{
    AffectedHit, AffectedParams, AffectedResult, FeedState, Fleet, FleetGroup, FleetProject, InvestigateParams, InvestigateResult, JudgeParams, JudgeResult, JudgeVerdict, MuteAddParams,
    MuteListResult, MuteRemoveParams, MuteRule, SentinelStatus, SweepParams, SweepProgress, SweepRecord, SweepStarted, TriageParams, TriageResult, methods, topics,
};
use cc_protocol::contracts::now_ms;
use cc_protocol::{Ctx, PluginEnv, RpcError, codes, parse_params, to_value};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::Notify;

const MINUTE_MS: u64 = 60_000;
const DAY_MS: u64 = 86_400_000;
/// The longest one sweep's tool may take before it is stopped (a computer with many large trees).
const SWEEP_LIMIT: Duration = Duration::from_secs(45 * 60);
/// The title the per-project dependency watch routines had; the sentinel replaces them.
const OLD_WATCH_TITLE: &str = "Security: dependency watch";

#[derive(Debug, Clone)]
struct Settings {
    investigate: bool,
    validate: bool,
    delta_ms: u64,
    full_ms: u64,
    /// `alerts` (urgent at once, the rest in a daily digest), `urgent` (only urgent ones), `off`.
    notify: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self { investigate: true, validate: false, delta_ms: 15 * MINUTE_MS, full_ms: DAY_MS, notify: "alerts".into() }
    }
}

#[derive(Debug, Clone)]
struct Request {
    project_id: Option<String>,
    full: bool,
}

#[derive(Debug, Clone)]
struct Active {
    project_id: String,
    finding_id: String,
}

#[derive(Default)]
struct State {
    saved: Saved,
    loaded: bool,
    settings: Settings,
    running: Option<SweepProgress>,
    requests: VecDeque<Request>,
    last: Option<SweepRecord>,
    /// Projects whose findings changed since the filter last looked; `all` after a sweep.
    dirty: HashSet<String>,
    all_dirty: bool,
    judging: u32,
    queue: VecDeque<(String, String)>,
    active: Option<Active>,
    machine: Vec<String>,
}

pub struct Sentinel {
    env: PluginEnv,
    ctx: OnceLock<Ctx>,
    state: Mutex<State>,
    wake: Notify,
    started: AtomicBool,
    stopped: AtomicBool,
    judging: AtomicBool,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Sentinel {
    pub fn new(env: PluginEnv) -> Arc<Self> {
        Arc::new(Self {
            env,
            ctx: OnceLock::new(),
            state: Mutex::new(State { all_dirty: true, ..Default::default() }),
            wake: Notify::new(),
            started: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            judging: AtomicBool::new(false),
        })
    }

    pub fn attach(&self, ctx: &Ctx) {
        let _ = self.ctx.set(ctx.clone());
    }

    fn ctx(&self) -> Result<&Ctx, RpcError> {
        self.ctx.get().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "the security sentinel is not connected yet"))
    }

    /// The data folder in use: the live one when it exists, else the package's.
    fn data_dir(&self) -> Option<PathBuf> {
        let packaged = cc_lang_kit::skills::locate_dir()?.parent()?.to_path_buf();
        Some(self.env.live_or("data", packaged))
    }

    fn script(&self) -> Result<PathBuf, RpcError> {
        let script = self.data_dir().map(|dir| dir.join("scripts").join("secaudit.py"));
        script.filter(|path| path.is_file()).ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "the secaudit tool (data/scripts/secaudit.py) is missing"))
    }

    fn triage_dir(&self) -> Result<PathBuf, RpcError> {
        let dir = self.data_dir().map(|dir| dir.join("triage"));
        dir.filter(|path| path.join("system.md").is_file()).ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "the filter's decision cards (data/triage) are missing"))
    }

    /// The probe's check: the tool and the cards are there.
    pub fn tool_files(&self) -> Result<String, String> {
        let script = self.script().map_err(|e| e.message)?;
        let cards = self.triage_dir().map_err(|e| e.message)?;
        let count = std::fs::read_dir(&cards).map(|dir| dir.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("card-")).count()).unwrap_or(0);
        Ok(format!("{} and {count} decision cards", script.display()))
    }

    pub fn start(self: &Arc<Self>, ctx: Ctx) {
        self.attach(&ctx);
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let sentinel = self.clone();
        tokio::spawn(async move { sentinel.run().await });
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    pub async fn event(self: &Arc<Self>, topic: &str, data: Value) {
        if topic == memory_topics::SECURITY_CHANGED {
            if let Some(project) = data.get("project_id").and_then(Value::as_str) {
                lock(&self.state).dirty.insert(project.to_owned());
            }
        }
        self.wake.notify_one();
    }

    // ---- the loop ---------------------------------------------------------------------------------

    async fn run(self: Arc<Self>) {
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            if let Err(error) = self.ensure_loaded().await {
                tracing::debug!(error = %error.message, "security sentinel: the saved state is not readable yet");
            }
            self.read_settings().await;
            if let Some(request) = self.next_request(now_ms()) {
                self.sweep(request).await;
            }
            self.judge_dirty().await;
            self.advance_investigations().await;
            self.digest().await;
            let wait = self.until_next(now_ms());
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    async fn ensure_loaded(&self) -> Result<(), RpcError> {
        if lock(&self.state).loaded {
            return Ok(());
        }
        let saved = store::load(self.ctx()?).await?;
        let mut state = lock(&self.state);
        state.saved = saved;
        state.loaded = true;
        Ok(())
    }

    async fn read_settings(&self) {
        let Ok(ctx) = self.ctx() else { return };
        let Ok(answer) = ctx.call(cc_protocol::contracts::memory::methods::SETTINGS_LIST, json!({"prefix": "security"})).await else { return };
        let tree = answer.get("settings").cloned().unwrap_or(Value::Null);
        let section = tree.get("security").cloned().unwrap_or(tree);
        let mut settings = Settings::default();
        if let Some(value) = section.get("investigate").and_then(Value::as_bool) {
            settings.investigate = value;
        }
        if let Some(value) = section.get("validate_secrets").and_then(Value::as_bool) {
            settings.validate = value;
        }
        if let Some(minutes) = section.get("delta_minutes").and_then(Value::as_u64).filter(|m| *m >= 1) {
            settings.delta_ms = minutes * MINUTE_MS;
        }
        if let Some(hours) = section.get("full_hours").and_then(Value::as_u64).filter(|h| *h >= 1) {
            settings.full_ms = hours * 60 * MINUTE_MS;
        }
        if let Some(notify) = section.get("notify").and_then(Value::as_str).filter(|n| matches!(*n, "alerts" | "urgent" | "off")) {
            settings.notify = notify.to_owned();
        }
        lock(&self.state).settings = settings;
    }

    fn next_request(&self, now: u64) -> Option<Request> {
        let mut state = lock(&self.state);
        if !state.loaded {
            return None;
        }
        if let Some(request) = state.requests.pop_front() {
            return Some(request);
        }
        let saved = &state.saved;
        if saved.last_full_ms == 0 || now >= saved.last_full_ms + state.settings.full_ms {
            return Some(Request { project_id: None, full: true });
        }
        if now >= saved.last_delta_ms.max(saved.last_full_ms) + state.settings.delta_ms {
            return Some(Request { project_id: None, full: false });
        }
        None
    }

    fn until_next(&self, now: u64) -> Duration {
        let state = lock(&self.state);
        if !state.loaded || !state.requests.is_empty() {
            return Duration::from_secs(60);
        }
        let saved = &state.saved;
        let full = saved.last_full_ms + state.settings.full_ms;
        let delta = saved.last_delta_ms.max(saved.last_full_ms) + state.settings.delta_ms;
        let next = full.min(delta);
        Duration::from_millis(next.saturating_sub(now).clamp(1_000, state.settings.delta_ms))
    }

    fn progress(&self, progress: Option<SweepProgress>) {
        if let (Some(ctx), Some(p)) = (self.ctx.get(), progress.as_ref()) {
            ctx.publish(topics::PROGRESS, serde_json::to_value(p).unwrap_or(Value::Null));
        }
        lock(&self.state).running = progress.filter(|p| !matches!(p.phase.as_str(), "done" | "failed"));
    }

    // ---- a sweep ----------------------------------------------------------------------------------

    async fn sweep(&self, request: Request) {
        let started = now_ms();
        let kind = if request.project_id.is_some() { "project" } else if request.full { "full" } else { "delta" };
        let mut record = SweepRecord { kind: kind.into(), started_ms: started, ..Default::default() };
        let mut progress = SweepProgress { kind: kind.into(), phase: "projects".into(), started_ms: started, note: "reading the projects".into(), ..Default::default() };
        self.progress(Some(progress.clone()));
        let outcome = self.sweep_inner(&request, &mut record, &mut progress).await;
        if let Err(error) = &outcome {
            record.errors.push(error.message.clone());
        }
        record.finished_ms = now_ms();
        {
            let mut state = lock(&self.state);
            let now = record.finished_ms;
            if request.project_id.is_none() {
                if request.full {
                    state.saved.last_full_ms = now;
                }
                state.saved.last_delta_ms = now;
            }
            state.last = Some(record.clone());
            state.all_dirty = true;
        }
        let saved = lock(&self.state).saved.clone();
        if let Ok(ctx) = self.ctx() {
            if let Err(error) = store::save(ctx, &saved).await {
                tracing::warn!(error = %error.message, "security sentinel: its state was not saved");
            }
            ctx.publish(topics::CHANGED, json!({}));
        }
        progress.phase = if outcome.is_ok() { "done" } else { "failed" }.into();
        progress.note = match &outcome {
            Ok(()) => format!("{} projects, {} packages, {} findings written", record.projects, record.packages, record.findings),
            Err(error) => error.message.clone(),
        };
        self.progress(Some(progress));
    }

    async fn sweep_inner(&self, request: &Request, record: &mut SweepRecord, progress: &mut SweepProgress) -> Result<(), RpcError> {
        let ctx = self.ctx()?.clone();
        let script = self.script()?;
        let mut projects = store::projects(&ctx).await?;
        if let Some(only) = &request.project_id {
            projects.retain(|p| &p.project_id == only);
        }
        if projects.is_empty() {
            return Ok(());
        }
        let (cursor, stamps, validate) = {
            let state = lock(&self.state);
            (state.saved.cursor_ms, state.saved.stamps.clone(), state.settings.validate)
        };
        let full = request.full || request.project_id.is_some() || cursor == 0;
        let mut args: Vec<String> = vec!["fleet".into(), "--json".into(), "--root".into()];
        args.extend(projects.iter().map(|p| p.root.clone()));
        if full {
            args.push("--full".into());
            let given: Vec<String> = projects.iter().filter_map(|p| stamps.get(&p.project_id).map(|stamp| format!("{}={stamp}", p.root))).collect();
            if !given.is_empty() {
                args.push("--stamp".into());
                args.extend(given);
            }
        } else {
            args.extend(["--since".into(), cursor.to_string()]);
        }
        if validate {
            args.push("--validate".into());
        }
        progress.phase = "advisories".into();
        progress.total = projects.len() as u32;
        progress.note = format!("{} the packages of {} projects", if full { "checking every tree and" } else { "re-checking" }, projects.len());
        self.progress(Some(progress.clone()));
        let output = run_tool(&script, &args, SWEEP_LIMIT).await?;
        let answer: Value = serde_json::from_slice(&output).map_err(|error| RpcError::internal(format!("the sweep's answer is not JSON ({error})")))?;
        if let Some(error) = answer.get("error").and_then(Value::as_str) {
            return Err(RpcError::internal(error.to_owned()));
        }
        record.packages = answer.get("packages").and_then(Value::as_u64).unwrap_or(0) as u32;
        let mode_full = answer.get("mode").and_then(Value::as_str) == Some("full");
        let now = now_ms();
        // Feeds and the cursor.
        {
            let mut state = lock(&self.state);
            if let Some(cursor) = answer.get("cursor_ms").and_then(Value::as_u64).filter(|c| *c > 0) {
                state.saved.cursor_ms = cursor;
            }
            for feed in answer.get("feeds").and_then(Value::as_array).into_iter().flatten() {
                let name = feed.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                let ok = feed.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let note = feed.get("note").and_then(Value::as_str).unwrap_or_default().to_owned();
                if !ok {
                    record.errors.push(note.clone());
                }
                let previous = state.saved.feeds.iter().find(|f| f.name == name).and_then(|f| f.checked_ms);
                let entry = FeedState { name: name.clone(), checked_ms: if ok { Some(now) } else { previous }, ok, note };
                match state.saved.feeds.iter_mut().find(|f| f.name == name) {
                    Some(slot) => *slot = entry,
                    None => state.saved.feeds.push(entry),
                }
            }
        }
        // Each project's findings.
        progress.phase = "findings".into();
        let by_root: HashMap<String, &cc_protocol::contracts::memory::ProjectInfo> = projects.iter().map(|p| (p.root.clone(), p)).collect();
        for (index, tree) in answer.get("projects").and_then(Value::as_array).into_iter().flatten().enumerate() {
            let root = tree.get("root").and_then(Value::as_str).unwrap_or_default();
            let Some(project) = by_root.get(root) else { continue };
            progress.done = index as u32 + 1;
            progress.note = format!("writing the findings of {}", project.name);
            self.progress(Some(progress.clone()));
            record.projects += 1;
            for error in tree.get("errors").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                record.errors.push(format!("{}: {error}", project.name));
            }
            let findings: Vec<Value> = tree.get("findings").and_then(Value::as_array).cloned().unwrap_or_default();
            match store::put_findings(&ctx, &project.project_id, &findings).await {
                Ok(kept) => record.findings += kept as u32,
                Err(error) => {
                    record.errors.push(format!("{}: the findings were not written ({})", project.name, error.message));
                    continue;
                }
            }
            let complete: Vec<&str> = tree.get("complete").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            if mode_full && !complete.is_empty() {
                let current: HashSet<&str> = findings.iter().filter_map(|f| f.get("id").and_then(Value::as_str)).collect();
                if let Err(error) = self.reconcile(&ctx, &project.project_id, &current, &complete).await {
                    record.errors.push(format!("{}: {}", project.name, error.message));
                }
            }
            if mode_full && let Some(stamp) = tree.get("stamp").and_then(Value::as_str) {
                let files_read = complete.contains(&"files");
                let posture: Vec<String> = if files_read {
                    tree.get("posture").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
                } else {
                    lock(&self.state).saved.posture.get(&project.project_id).cloned().unwrap_or_default()
                };
                let packages = tree.get("packages").and_then(Value::as_u64).unwrap_or(0) as u32;
                // A tree whose file checks did not run keeps its old stamp, so the next sweep reads it.
                let keep = if files_read || tree.get("unchanged").and_then(Value::as_bool) == Some(true) { stamp.to_owned() } else { String::new() };
                if let Err(error) = store::save_watch(&ctx, &project.project_id, &keep, packages, &posture, now).await {
                    tracing::debug!(error = %error.message, "security sentinel: a project's watch record was not written");
                }
                let mut state = lock(&self.state);
                state.saved.stamps.insert(project.project_id.clone(), keep);
                state.saved.posture.insert(project.project_id.clone(), posture);
            }
        }
        if mode_full {
            self.machine_findings(&ctx, answer.get("machine")).await;
            self.move_routines(&ctx, &projects).await;
        }
        Ok(())
    }

    /// Close what a complete sweep no longer sees: a sweep finding by the host's own check, an older
    /// dependency-watch finding by its pinned-version evidence.
    async fn reconcile(&self, ctx: &Ctx, project_id: &str, current: &HashSet<&str>, complete: &[&str]) -> Result<(), RpcError> {
        let open = store::findings(ctx, project_id, &["open", "fixing"], None).await?;
        let mut recheck = Vec::new();
        for finding in open {
            if current.contains(finding.id.as_str()) {
                continue;
            }
            let dependency = finding.id.starts_with("watch:");
            let file_kind = ["supply:", "posture:", "ci:", "agent:", "secret:"].iter().any(|prefix| finding.id.starts_with(prefix));
            if (dependency && !complete.contains(&"dependencies")) || (file_kind && !complete.contains(&"files")) || !(dependency || file_kind) {
                continue;
            }
            if finding.source == "sweep" {
                if let Err(error) = store::close_swept(ctx, project_id, &finding.id).await {
                    tracing::debug!(error = %error.message, id = %finding.id, "security sentinel: a swept finding was not closed");
                }
            } else if dependency {
                recheck.push(finding.id.clone());
            }
        }
        store::recheck(ctx, project_id, &recheck).await
    }

    /// Campaign persistence on the computer itself: told at once, once per path.
    async fn machine_findings(&self, ctx: &Ctx, machine: Option<&Value>) {
        let hits: Vec<(String, String, String)> = machine
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|hit| {
                let field = |key: &str| hit.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
                (field("path"), field("campaign"), field("why"))
            })
            .collect();
        let lines: Vec<String> = hits.iter().map(|(path, campaign, why)| format!("{path}: {why} ({campaign})")).collect();
        let new: Vec<(String, String, String)> = {
            let mut state = lock(&self.state);
            state.machine = lines;
            let told: HashSet<String> = state.saved.machine_told.iter().cloned().collect();
            let fresh: Vec<_> = hits.iter().filter(|(path, _, _)| !told.contains(path)).cloned().collect();
            state.saved.machine_told = hits.iter().map(|(path, _, _)| path.clone()).collect();
            fresh
        };
        for (path, campaign, why) in new {
            let notification = Notification {
                kind: kinds::REPORT.into(),
                title: format!("Security: {campaign} on this computer"),
                body: format!("{path}: {why}. Remove it, then rotate the GitHub, npm and cloud credentials this computer holds."),
                key: Some(format!("security:machine:{path}")),
                project_id: None,
                session_id: None,
                target: json!({"view": "security"}),
                actions: vec!["Open".into()],
                subtitle: Some(path.clone()),
            };
            if let Err(error) = ctx.call(notify_methods::SEND, serde_json::to_value(&notification).unwrap_or(Value::Null)).await {
                tracing::warn!(error = %error.message, "security sentinel: a machine alert was not sent");
            }
        }
    }

    /// The per-project dependency watch routines an earlier version made: the sentinel watches every
    /// project now, so they end (once).
    async fn move_routines(&self, ctx: &Ctx, projects: &[cc_protocol::contracts::memory::ProjectInfo]) {
        if lock(&self.state).saved.routines_moved {
            return;
        }
        for project in projects {
            let Ok(listed) = ctx.call(cc_protocol::contracts::schedule::methods::LIST, json!({"scope": project.project_id})).await else { return };
            for routine in listed.get("routines").and_then(Value::as_array).into_iter().flatten() {
                let title = routine.get("title").and_then(Value::as_str).unwrap_or_default();
                let ended = routine.get("state").and_then(Value::as_str) == Some("ended");
                if title == OLD_WATCH_TITLE && !ended {
                    if let Some(id) = routine.get("id").and_then(Value::as_str) {
                        let _ = ctx.call(cc_protocol::contracts::schedule::methods::REMOVE, json!({"id": id})).await;
                    }
                }
            }
        }
        lock(&self.state).saved.routines_moved = true;
    }

    // ---- the filter -------------------------------------------------------------------------------

    async fn judge_dirty(&self) {
        let (all, dirty) = {
            let mut state = lock(&self.state);
            let all = std::mem::take(&mut state.all_dirty);
            (all, std::mem::take(&mut state.dirty))
        };
        if !all && dirty.is_empty() {
            return;
        }
        let Ok(ctx) = self.ctx() else { return };
        let Ok(projects) = store::projects(ctx).await else {
            let mut state = lock(&self.state);
            state.all_dirty = all;
            state.dirty.extend(dirty);
            return;
        };
        for project in projects.iter().filter(|p| all || dirty.contains(&p.project_id)) {
            if let Err(error) = self.judge_project(&project.project_id, &project.name, None, false).await {
                tracing::debug!(error = %error.message, project = %project.name, "security sentinel: findings not judged");
            }
        }
    }

    /// Judge a project's findings not judged yet (or `ids`, again when asked), then tell what is
    /// urgent. One pass at a time; a second caller finds the work done or waiting.
    async fn judge_project(&self, project_id: &str, project_name: &str, ids: Option<&[String]>, again: bool) -> Result<TriageResult, RpcError> {
        if self.judging.swap(true, Ordering::SeqCst) {
            lock(&self.state).dirty.insert(project_id.to_owned());
            return Ok(TriageResult::default());
        }
        let outcome = self.judge_inner(project_id, project_name, ids, again).await;
        self.judging.store(false, Ordering::SeqCst);
        outcome
    }

    async fn judge_inner(&self, project_id: &str, project_name: &str, ids: Option<&[String]>, again: bool) -> Result<TriageResult, RpcError> {
        let ctx = self.ctx()?.clone();
        let rules = store::mutes(&ctx).await?;
        let now = now_ms();
        let investigate = lock(&self.state).settings.investigate;
        let mut list: Vec<SecurityFinding> = match ids {
            Some(ids) => store::findings(&ctx, project_id, &["open", "fixing"], None).await?.into_iter().filter(|f| ids.contains(&f.id)).collect(),
            None => store::findings(&ctx, project_id, &["open", "fixing"], Some(&["pending"])).await?,
        };
        if !again {
            list.retain(|f| f.triage.is_none());
        }
        // A muted finding whose rule is gone or ended is judged again.
        if ids.is_none() {
            let muted = store::findings(&ctx, project_id, &["open", "fixing"], Some(&["muted"])).await?;
            let live: HashSet<&str> = rules.iter().filter(|r| r.until_ms.is_none_or(|until| until > now)).map(|r| r.key.as_str()).collect();
            list.extend(muted.into_iter().filter(|f| f.muted_by.as_deref().is_none_or(|key| !live.contains(key))));
        }
        let mut result = TriageResult::default();
        lock(&self.state).judging = list.len() as u32;
        let cards_dir = self.triage_dir().ok();
        let mut model_up = true;
        for finding in list {
            let judgment = if let Some(rule) = rules.iter().find(|rule| triage::mute_covers(rule, project_id, &finding, now)) {
                Some(Judgment { triage: "muted".into(), reason: if rule.note.is_empty() { format!("muted: {}", rule.label) } else { format!("muted: {}", rule.note) }, by: "host".into(), muted_by: Some(rule.key.clone()) })
            } else if let Some(judgment) = triage::host_rule(&finding) {
                Some(judgment)
            } else if let (Some(dir), true) = (cards_dir.as_deref(), model_up) {
                match triage::model_judgment(&ctx, &Cards { dir }, &finding, investigate).await {
                    Ok(judgment) => Some(judgment),
                    Err(error) => {
                        // The model is away: the rest wait for the next pass, still shown as not judged.
                        tracing::debug!(error = %error.message, "security sentinel: the model did not judge");
                        model_up = !matches!(error.code.as_str(), codes::UNAVAILABLE | codes::TIMEOUT);
                        None
                    }
                }
            } else {
                None
            };
            let Some(judgment) = judgment else { continue };
            if finding.triage.as_deref() == Some(judgment.triage.as_str()) && !again && judgment.triage != "muted" {
                continue;
            }
            let item = SecurityTriage {
                id: finding.id.clone(),
                triage: judgment.triage.clone(),
                reason: judgment.reason.clone(),
                by: judgment.by.clone(),
                muted_by: judgment.muted_by.clone(),
                ..Default::default()
            };
            if store::judge(&ctx, project_id, vec![item]).await.is_ok() {
                result.judged += 1;
                match judgment.triage.as_str() {
                    "alert" => result.alerts += 1,
                    "investigating" => {
                        result.investigating += 1;
                        lock(&self.state).queue.push_back((project_id.to_owned(), finding.id.clone()));
                    }
                    _ => result.quiet += 1,
                }
            }
            let mut state = lock(&self.state);
            state.judging = state.judging.saturating_sub(1);
        }
        lock(&self.state).judging = 0;
        self.tell_urgent(&ctx, project_id, project_name).await;
        Ok(result)
    }

    /// Tell the person about urgent alerts not told yet, one notification each.
    async fn tell_urgent(&self, ctx: &Ctx, project_id: &str, project_name: &str) {
        if lock(&self.state).settings.notify == "off" {
            return;
        }
        let Ok(alerts) = store::findings(ctx, project_id, &["open", "fixing"], Some(&["alert"])).await else { return };
        for finding in alerts.into_iter().filter(|f| f.notified_ms.is_none() && triage::urgent(f)) {
            let severity = finding.severity_overall.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "high".into());
            let notification = Notification {
                kind: kinds::REPORT.into(),
                title: format!("Security: {}", finding.title),
                body: format!("{} ({severity}). Open the Security tab to act on it.", finding.triage_reason.clone().unwrap_or_default().trim_end_matches('.')),
                key: Some(format!("security:{project_id}:{}", finding.id)),
                project_id: Some(project_id.to_owned()),
                session_id: None,
                target: json!({"project_id": project_id, "view": "security"}),
                actions: vec!["Open".into()],
                subtitle: Some(project_name.to_owned()),
            };
            if ctx.call(notify_methods::SEND, serde_json::to_value(&notification).unwrap_or(Value::Null)).await.is_ok() {
                let item = SecurityTriage { id: finding.id.clone(), triage: "alert".into(), reason: finding.triage_reason.clone().unwrap_or_default(), by: finding.triage_by.clone().unwrap_or_else(|| "host".into()), notified: true, ..Default::default() };
                let _ = store::judge(ctx, project_id, vec![item]).await;
            }
        }
    }

    /// Once a day, one notification for the alerts that were not urgent enough to tell at once.
    async fn digest(&self) {
        let (notify, last) = {
            let state = lock(&self.state);
            (state.settings.notify.clone(), state.saved.digest_ms)
        };
        let now = now_ms();
        if notify != "alerts" || now < last + DAY_MS || !lock(&self.state).loaded {
            return;
        }
        let Ok(ctx) = self.ctx() else { return };
        let Ok(projects) = store::projects(ctx).await else { return };
        let mut waiting: Vec<(String, SecurityFinding)> = Vec::new();
        for project in &projects {
            if let Ok(alerts) = store::findings(ctx, &project.project_id, &["open", "fixing"], Some(&["alert"])).await {
                waiting.extend(alerts.into_iter().filter(|f| f.notified_ms.is_none()).map(|f| (project.project_id.clone(), f)));
            }
        }
        lock(&self.state).saved.digest_ms = now;
        if waiting.is_empty() {
            return;
        }
        let projects_with: HashSet<&str> = waiting.iter().map(|(p, _)| p.as_str()).collect();
        let first = &waiting[0].1;
        let notification = Notification {
            kind: kinds::INFO.into(),
            title: format!("Security: {} alert{} to look at", waiting.len(), if waiting.len() == 1 { "" } else { "s" }),
            body: format!(
                "In {} project{}; the filter set aside everything else. First: {}.",
                projects_with.len(),
                if projects_with.len() == 1 { "" } else { "s" },
                first.title
            ),
            key: Some("security:digest".into()),
            project_id: None,
            session_id: None,
            target: json!({"view": "security", "scope": "fleet"}),
            actions: vec!["Open".into()],
            subtitle: None,
        };
        if ctx.call(notify_methods::SEND, serde_json::to_value(&notification).unwrap_or(Value::Null)).await.is_ok() {
            let mut by_project: HashMap<String, Vec<SecurityTriage>> = HashMap::new();
            for (project, finding) in waiting {
                by_project.entry(project).or_default().push(SecurityTriage {
                    id: finding.id.clone(),
                    triage: "alert".into(),
                    reason: finding.triage_reason.clone().unwrap_or_default(),
                    by: finding.triage_by.clone().unwrap_or_else(|| "host".into()),
                    notified: true,
                    ..Default::default()
                });
            }
            for (project, items) in by_project {
                let _ = store::judge(ctx, &project, items).await;
            }
        }
        let saved = lock(&self.state).saved.clone();
        let _ = store::save(ctx, &saved).await;
    }

    // ---- investigations ---------------------------------------------------------------------------

    async fn advance_investigations(&self) {
        let Ok(ctx) = self.ctx().cloned() else { return };
        let active = lock(&self.state).active.clone();
        if let Some(active) = active {
            match store::finding(&ctx, &active.project_id, &active.finding_id).await {
                Ok(finding) if finding.triage.as_deref() == Some("investigating") => return,
                _ => lock(&self.state).active = None,
            }
        }
        if !lock(&self.state).settings.investigate {
            return;
        }
        if lock(&self.state).queue.is_empty() {
            // After a restart: findings left investigating with no session yet.
            if let Ok(projects) = store::projects(&ctx).await {
                for project in projects {
                    if let Ok(list) = store::findings(&ctx, &project.project_id, &["open", "fixing"], Some(&["investigating"])).await {
                        let mut state = lock(&self.state);
                        for finding in list.into_iter().filter(|f| f.investigation_session.is_none()) {
                            state.queue.push_back((project.project_id.clone(), finding.id));
                        }
                    }
                }
            }
        }
        loop {
            let Some((project_id, id)) = lock(&self.state).queue.pop_front() else { return };
            let Ok(finding) = store::finding(&ctx, &project_id, &id).await else { continue };
            if finding.triage.as_deref() != Some("investigating") || finding.investigation_session.is_some() {
                continue;
            }
            match self.open_investigation(&ctx, &project_id, &finding).await {
                Ok(()) => {
                    lock(&self.state).active = Some(Active { project_id, finding_id: id });
                    return;
                }
                Err(error) => {
                    // A session that cannot start leaves the finding shown.
                    let item = SecurityTriage { id: id.clone(), triage: "alert".into(), reason: format!("could not be investigated ({}), so it stays shown", error.message), by: "host".into(), ..Default::default() };
                    let _ = store::judge(&ctx, &project_id, vec![item]).await;
                }
            }
        }
    }

    async fn open_investigation(&self, ctx: &Ctx, project_id: &str, finding: &SecurityFinding) -> Result<(), RpcError> {
        let template = self.triage_dir().ok().and_then(|dir| Cards { dir: &dir }.investigation()).ok_or_else(|| RpcError::internal("the investigation brief (data/triage/investigate.md) is missing"))?;
        let place = match (finding.sink_path.as_deref(), finding.sink_line) {
            (Some(path), Some(line)) if line > 0 => format!("{path}:{line}"),
            (Some(path), _) => path.to_owned(),
            _ => "not named".into(),
        };
        let facts = triage::facts(finding).into_iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n");
        let text = template
            .replace("{id}", &finding.id)
            .replace("{title}", &finding.title)
            .replace("{class}", &finding.class)
            .replace("{severity}", finding.severity_overall.as_deref().unwrap_or("not rated"))
            .replace("{source}", &finding.source)
            .replace("{place}", &place)
            .replace("{observed}", &finding.evidence_observed)
            .replace("{reason}", finding.triage_reason.as_deref().unwrap_or(""))
            .replace("{facts}", &facts);
        let title: String = format!("Investigate: {}", finding.title).chars().take(80).collect();
        let session = ctx.call("frame.session.create", json!({"project_id": project_id, "kind": "chat", "title": title})).await?;
        let session_id = session.get("id").and_then(Value::as_str).ok_or_else(|| RpcError::internal("frame.session.create gave no session id"))?.to_owned();
        let flow = session.get("flow").and_then(Value::as_str).filter(|f| !f.is_empty()).unwrap_or("coding").to_owned();
        let item = SecurityTriage {
            id: finding.id.clone(),
            triage: "investigating".into(),
            reason: finding.triage_reason.clone().unwrap_or_default(),
            by: finding.triage_by.clone().unwrap_or_else(|| "model".into()),
            investigation_session: Some(session_id.clone()),
            ..Default::default()
        };
        store::judge(ctx, project_id, vec![item]).await?;
        // The turn runs on its own; the verdict comes back through the finding.
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(error) = ctx.call(&format!("chatflow-{flow}.turn"), json!({"session_id": session_id, "text": text, "mode": "auto"})).await {
                tracing::warn!(error = %error.message, "security sentinel: the investigation turn did not start");
            }
        });
        Ok(())
    }

    // ---- methods ----------------------------------------------------------------------------------

    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            methods::STATUS => to_value(&self.status()),
            methods::FLEET => to_value(&self.fleet().await?),
            methods::SWEEP => {
                let p: SweepParams = parse_params(params)?;
                let running = {
                    let mut state = lock(&self.state);
                    let running = state.running.is_some();
                    let duplicate = state.requests.iter().any(|r| r.project_id == p.project_id && r.full == p.full);
                    if !duplicate {
                        state.requests.push_back(Request { project_id: p.project_id, full: p.full });
                    }
                    running
                };
                self.wake.notify_one();
                to_value(&SweepStarted { started: !running, running, queued: running })
            }
            methods::TRIAGE => {
                let p: TriageParams = parse_params(params)?;
                let name = store::projects(self.ctx()?).await?.into_iter().find(|proj| proj.project_id == p.project_id).map(|proj| proj.name).unwrap_or_default();
                let result = self.judge_project(&p.project_id, &name, p.ids.as_deref(), p.again).await?;
                self.wake.notify_one();
                to_value(&result)
            }
            methods::INVESTIGATE => {
                let p: InvestigateParams = parse_params(params)?;
                let ctx = self.ctx()?;
                let finding = store::finding(ctx, &p.project_id, &p.id).await?;
                let item = SecurityTriage {
                    id: finding.id.clone(),
                    triage: "investigating".into(),
                    reason: "the person asked for an investigation".into(),
                    by: "user".into(),
                    ..Default::default()
                };
                store::judge(ctx, &p.project_id, vec![item]).await?;
                let busy = lock(&self.state).active.is_some();
                lock(&self.state).queue.push_front((p.project_id.clone(), p.id.clone()));
                self.advance_investigations().await;
                let session_id = store::finding(ctx, &p.project_id, &p.id).await.ok().and_then(|f| f.investigation_session);
                to_value(&InvestigateResult { queued: busy && session_id.is_none(), session_id })
            }
            methods::MUTE_ADD => to_value(&self.mute_add(parse_params(params)?).await?),
            methods::MUTE_LIST => to_value(&MuteListResult { rules: store::mutes(self.ctx()?).await? }),
            methods::MUTE_REMOVE => {
                let p: MuteRemoveParams = parse_params(params)?;
                store::mute_retire(self.ctx()?, &p.key).await?;
                lock(&self.state).all_dirty = true;
                self.wake.notify_one();
                if let Ok(ctx) = self.ctx() {
                    ctx.publish(topics::CHANGED, json!({}));
                }
                to_value(&json!({}))
            }
            methods::AFFECTED => to_value(&self.affected(parse_params(params)?).await?),
            methods::JUDGE => to_value(&self.judge_external(parse_params(params)?).await?),
            other => Err(RpcError::unknown_method(other)),
        }
    }

    fn status(&self) -> SentinelStatus {
        let state = lock(&self.state);
        let saved = &state.saved;
        SentinelStatus {
            running: state.running.clone(),
            last: state.last.clone(),
            next_delta_ms: Some(saved.last_delta_ms.max(saved.last_full_ms) + state.settings.delta_ms).filter(|_| state.loaded),
            next_full_ms: Some(saved.last_full_ms + state.settings.full_ms).filter(|_| state.loaded),
            feeds: saved.feeds.clone(),
            judging: state.judging,
            investigations: state.queue.len() as u32 + u32::from(state.active.is_some()),
            machine: state.machine.clone(),
        }
    }

    async fn fleet(&self) -> Result<Fleet, RpcError> {
        let ctx = self.ctx()?;
        let projects = store::projects(ctx).await?;
        let posture = lock(&self.state).saved.posture.clone();
        let mut out = Vec::new();
        // fleet key -> (title, worst severity rank, worst severity, any alert, projects, fix)
        let mut groups: BTreeMap<String, (String, i32, String, bool, Vec<String>, Option<String>)> = BTreeMap::new();
        for project in projects {
            let mut row = FleetProject { project_id: project.project_id.clone(), name: project.name.clone(), root: project.root.clone(), ..Default::default() };
            row.posture = posture.get(&project.project_id).cloned().unwrap_or_default();
            match (store::summary(ctx, &project.project_id).await, store::findings(ctx, &project.project_id, &["open", "fixing"], None).await) {
                (Ok(summary), Ok(findings)) => {
                    row.open = summary.open + summary.fixing;
                    row.alerts = summary.alerts;
                    row.investigating = summary.investigating;
                    row.quiet = summary.quiet;
                    row.pending = summary.pending;
                    row.last_audit_ms = summary.last_audit.map(|audit| audit.finished_ms.unwrap_or(audit.started_ms));
                    for finding in &findings {
                        let alert = finding.triage.as_deref() == Some("alert");
                        if alert {
                            *row.by_severity.entry(finding.severity_overall.clone().unwrap_or_else(|| "unrated".into())).or_insert(0) += 1;
                            if triage::urgent(finding) {
                                row.urgent += 1;
                            }
                        }
                        let Some(key) = finding.fleet_key.clone() else { continue };
                        let severity = finding.severity_overall.clone().unwrap_or_default();
                        let rank = severity_rank(&severity);
                        let fix = finding.quick_fix.as_ref().map(|fix| format!("{} {}", fix.package, fix.to));
                        let entry = groups.entry(key).or_insert_with(|| (finding.title.clone(), rank, severity.clone(), false, Vec::new(), fix.clone()));
                        if rank > entry.1 {
                            entry.1 = rank;
                            entry.2 = severity;
                        }
                        entry.3 |= alert;
                        if !entry.4.contains(&project.project_id) {
                            entry.4.push(project.project_id.clone());
                        }
                        if entry.5.is_none() {
                            entry.5 = fix;
                        }
                    }
                }
                (Err(error), _) | (_, Err(error)) => row.error = Some(error.message),
            }
            out.push(row);
        }
        out.sort_by(|a, b| (b.urgent, b.alerts, &a.name).cmp(&(a.urgent, a.alerts, &b.name)));
        let mut shared: Vec<FleetGroup> = groups
            .into_iter()
            .filter(|(_, group)| group.4.len() >= 2)
            .map(|(key, (title, _, severity, alert, project_ids, fix))| FleetGroup { key, title, severity, triage: if alert { "alert".into() } else { "quiet".into() }, project_ids, fix })
            .collect();
        shared.sort_by(|a, b| ((a.triage != "alert"), std::cmp::Reverse(severity_rank(&a.severity)), std::cmp::Reverse(a.project_ids.len())).cmp(&((b.triage != "alert"), std::cmp::Reverse(severity_rank(&b.severity)), std::cmp::Reverse(b.project_ids.len()))));
        Ok(Fleet { projects: out, groups: shared, status: self.status() })
    }

    async fn mute_add(&self, p: MuteAddParams) -> Result<MuteRule, RpcError> {
        let ctx = self.ctx()?;
        let finding = store::finding(ctx, &p.project_id, &p.finding_id).await?;
        let mut rule = MuteRule { kind: p.kind.clone(), note: p.note.trim().to_owned(), created_ms: now_ms(), until_ms: p.until_ms, ..Default::default() };
        match p.kind.as_str() {
            "finding" => {
                rule.project_id = Some(p.project_id.clone());
                rule.finding_id = Some(finding.id.clone());
                rule.label = format!("this finding: {}", finding.title);
            }
            "similar" => {
                let key = finding.fleet_key.clone().ok_or_else(|| RpcError::invalid_params("this finding has no counterpart in other projects; mute this finding instead"))?;
                rule.fleet_key = Some(key);
                rule.label = format!("the same problem in every project: {}", finding.title);
            }
            "package" => {
                let (eco, package) = triage::package_of(&finding).ok_or_else(|| RpcError::invalid_params("this finding names no package"))?;
                rule.label = format!("{package} ({eco}) in every project");
                rule.ecosystem = Some(eco);
                rule.package = Some(package);
            }
            "advisory" => {
                let advisory = finding.advisories.first().cloned().ok_or_else(|| RpcError::invalid_params("this finding names no advisory"))?;
                rule.label = format!("{advisory} everywhere");
                rule.advisory = Some(advisory);
            }
            "path" => {
                let path = finding.sink_path.clone().ok_or_else(|| RpcError::invalid_params("this finding names no file"))?;
                let folder = std::path::Path::new(&path).parent().map(|p| p.to_string_lossy().into_owned()).filter(|p| !p.is_empty()).unwrap_or(path);
                rule.project_id = Some(p.project_id.clone());
                rule.class = Some(finding.class.clone());
                rule.label = format!("{} under {folder}", finding.class);
                rule.path_prefix = Some(folder);
            }
            other => return Err(RpcError::invalid_params(format!("mute kind `{other}` is not one of finding, similar, package, advisory, path"))),
        }
        let identity = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}",
            rule.kind,
            rule.project_id.as_deref().unwrap_or(""),
            rule.finding_id.as_deref().unwrap_or(""),
            rule.fleet_key.as_deref().unwrap_or(""),
            rule.ecosystem.as_deref().unwrap_or(""),
            rule.package.as_deref().unwrap_or(""),
            rule.advisory.as_deref().unwrap_or(""),
            rule.class.as_deref().unwrap_or(""),
            rule.path_prefix.as_deref().unwrap_or("")
        );
        rule.key = format!("mute-{}", &cc_protocol::sha256::hex(identity.as_bytes())[..16]);
        store::mute_put(ctx, &rule).await?;
        // The findings it covers, in every project, are muted at once.
        let projects = store::projects(ctx).await?;
        let now = now_ms();
        for project in projects {
            let Ok(list) = store::findings(ctx, &project.project_id, &["open", "fixing"], None).await else { continue };
            let items: Vec<SecurityTriage> = list
                .into_iter()
                .filter(|f| triage::mute_covers(&rule, &project.project_id, f, now))
                .map(|f| SecurityTriage {
                    id: f.id,
                    triage: "muted".into(),
                    reason: if rule.note.is_empty() { format!("muted: {}", rule.label) } else { format!("muted: {}", rule.note) },
                    by: "user".into(),
                    muted_by: Some(rule.key.clone()),
                    ..Default::default()
                })
                .collect();
            let _ = store::judge(ctx, &project.project_id, items).await;
        }
        ctx.publish(topics::CHANGED, json!({}));
        Ok(rule)
    }

    async fn affected(&self, p: AffectedParams) -> Result<AffectedResult, RpcError> {
        let ctx = self.ctx()?;
        let projects = store::projects(ctx).await?;
        let mut args: Vec<String> = vec!["affected".into(), "--json".into(), "--query".into(), p.query.clone(), "--root".into()];
        args.extend(projects.iter().map(|p| p.root.clone()));
        let output = run_tool(&self.script()?, &args, Duration::from_secs(10 * 60)).await?;
        let answer: Value = serde_json::from_slice(&output).map_err(|error| RpcError::internal(format!("the answer is not JSON ({error})")))?;
        if let Some(error) = answer.get("error").and_then(Value::as_str) {
            return Err(RpcError::invalid_params(error.to_owned()));
        }
        let by_root: HashMap<&str, &cc_protocol::contracts::memory::ProjectInfo> = projects.iter().map(|p| (p.root.as_str(), p)).collect();
        let hits = answer
            .get("hits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|hit| {
                let project = by_root.get(hit.get("root").and_then(Value::as_str)?)?;
                Some(AffectedHit {
                    project_id: project.project_id.clone(),
                    name: project.name.clone(),
                    what: hit.get("what").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    path: hit.get("path").and_then(Value::as_str).unwrap_or_default().to_owned(),
                })
            })
            .collect();
        Ok(AffectedResult {
            query: p.query,
            hits,
            checked: answer.get("checked").and_then(Value::as_u64).unwrap_or(0) as u32,
            note: answer.get("note").and_then(Value::as_str).unwrap_or_default().to_owned(),
        })
    }

    /// Another app's events, judged one by one with the external card.
    async fn judge_external(&self, p: JudgeParams) -> Result<JudgeResult, RpcError> {
        let ctx = self.ctx()?;
        let dir = self.triage_dir()?;
        let cards = Cards { dir: &dir };
        let system = cards.system()?;
        let card = cards.card("external");
        let mut verdicts = Vec::new();
        for item in p.items {
            let mut facts = vec![format!("Event: {}", item.title), format!("Kind: {}", item.kind)];
            facts.extend(item.facts.iter().map(|f| f.chars().take(400).collect::<String>()));
            let (decision, reason) = triage::ask(ctx, &system, &card, &facts, &["dismiss", "hardening", "alert", "investigate"]).await?;
            let decision = if decision == "hardening" { "note".to_owned() } else { decision };
            verdicts.push(JudgeVerdict { id: item.id, decision, reason });
        }
        Ok(JudgeResult { verdicts })
    }
}

fn severity_rank(severity: &str) -> i32 {
    match severity {
        "critical" => 4,
        "high" => 3,
        "medium" | "moderate" => 2,
        "low" => 1,
        _ => 0,
    }
}

/// Run the secaudit tool; its standard output, or why it failed.
async fn run_tool(script: &std::path::Path, args: &[String], limit: Duration) -> Result<Vec<u8>, RpcError> {
    let mut command = tokio::process::Command::new("python3");
    command.arg("-I").arg(script).args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let output = match tokio::time::timeout(limit, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(RpcError::new(codes::UNAVAILABLE, format!("python3 could not start: {error}"))),
        Err(_) => return Err(RpcError::new(codes::TIMEOUT, format!("the security tool did not finish in {} minutes", limit.as_secs() / 60))),
    };
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        let error: String = String::from_utf8_lossy(&output.stderr).lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("no answer").chars().take(300).collect();
        return Err(RpcError::internal(format!("the security tool failed: {error}")));
    }
    Ok(output.stdout)
}
