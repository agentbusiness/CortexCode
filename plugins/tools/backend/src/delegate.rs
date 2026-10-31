//! Commands run by the CortexTerm on this Mac (its shared service `terminal.command`,
//! docs/STANDARD-CHANGES.md "delegated commands and schedules"). When a CortexTerm offers it, `run`
//! sends the command there: a fresh `bash -lc` in a shell of its own for this session, in the same
//! write sandbox (`sandbox-kit`), with the session's exported variables, venv and toolchains. Code
//! runs (the project's checks, background jobs) are traced: CPU, memory, processes, files. The
//! model gets the output as from its own shell, plus one line naming `cmd://<id>`, which it reads
//! for the status, the output, the machine's numbers, or what was happening at one moment, also
//! days later. Without a CortexTerm, or when it is restarting, nothing changes: `run` uses its own
//! shell.
//!
//! ```text
//! (:Command {key: provider's job id, provider, provider_name, command, cwd, purpose, background,
//!     monitored, state*, exit_code, started_ms, finished_ms, duration_ms, text, trace, trace_line,
//!     wake_pattern, wake_silence_s, session_id, project_id})
//!     -[:ON]->(:Machine)   -[:FOR]->(:Session)   -[:IN]->(:Project)
//! ```
//!
//! The node keeps what CortexTerm answered (its text and trace line); the output (30 days) and the
//! metrics (7 days) stay in CortexTerm.

use crate::host::Host;
use crate::shell::{Capture, RunOutput};
use crate::uris::UriError;
use crate::util;
use cc_protocol::contracts::service::{CommandEnded, CommandMatched, Job, Moment, Watch};
use cc_service_kit::client::{self, Asker, Provider, Ran};
use cc_service_kit::text;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const SERVICE: &str = "terminal.command";
const VERSION: u32 = 1;
/// How long the answer "is there a provider" is reused.
const FRESH: Duration = Duration::from_secs(15);
/// Reads of a job, its output or a moment.
const READ: Duration = Duration::from_secs(15);
/// Variables of a session's shell that are the machine's or the login shell's, not the session's.
const NOT_SENT: &[&str] = &["PATH", "HOME", "SHELL", "PWD", "OLDPWD", "SHLVL", "TERM", "_", "USER", "LOGNAME", "TMPDIR", "VIRTUAL_ENV", "PS1", "PS2", "PS4"];

static CACHE: Mutex<Option<(Instant, Option<Provider>)>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The CortexTerm on this Mac that runs commands, if there is one now.
pub async fn provider(host: &dyn Host) -> Option<Provider> {
    let cached = lock(&CACHE).clone();
    if let Some((at, known)) = &cached
        && at.elapsed() < FRESH
    {
        return known.clone();
    }
    let found = match host.link() {
        Some(link) => client::provider(&link, SERVICE, VERSION).await,
        None => None,
    };
    *lock(&CACHE) = Some((Instant::now(), found.clone()));
    // CortexTerm is back after it was away: its commands that ended meanwhile are asked for.
    if found.is_some() && cached.is_some_and(|(_, known)| known.is_none()) {
        CAME_BACK.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    found
}

/// Set when CortexTerm came back; the next `run` asks how the unfinished commands ended.
static CAME_BACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether CortexTerm came back since this was last asked.
pub fn came_back() -> bool {
    CAME_BACK.swap(false, std::sync::atomic::Ordering::SeqCst)
}

/// Forget the provider: it answered `unavailable` or `busy`.
pub fn forget() {
    *lock(&CACHE) = None;
}

/// One delegated command as this app keeps it.
#[derive(Debug, Clone, Default)]
pub struct CommandMeta {
    pub id: String,
    pub provider: String,
    pub provider_name: String,
    pub command: String,
    pub cwd: String,
    pub purpose: String,
    pub background: bool,
    pub monitored: bool,
    pub state: String,
    pub exit_code: Option<i32>,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub duration_ms: u64,
    pub text: String,
    pub trace: String,
    pub trace_line: String,
    pub wake_pattern: Option<String>,
    pub wake_silence_s: Option<u64>,
    pub session_id: String,
    pub project_id: String,
}

impl CommandMeta {
    fn take(&mut self, job: &Job) {
        if !job.id.is_empty() {
            self.id = job.id.clone();
        }
        self.state = job.state.clone();
        self.exit_code = job.exit_code;
        self.duration_ms = job.duration_ms;
        if job.started > 0 {
            self.started_ms = job.started;
        }
        self.finished_ms = job.ended;
        let text = text::job(job);
        if !text.is_empty() {
            self.text = text;
        }
        if let Some(trace) = &job.trace {
            self.trace = trace.trim_start_matches("trace://").to_owned();
        }
        if let Some(line) = &job.trace_line {
            self.trace_line = line.clone();
        }
    }

    fn batch(&self) -> crate::graph::Batch {
        use crate::graph;
        let me = graph::gref("Command", &self.id);
        let mut node = graph::node(
            "Command",
            &self.id,
            graph::props(json!({
                "provider": self.provider,
                "provider_name": self.provider_name,
                "command": self.command,
                "cwd": self.cwd,
                "purpose": self.purpose,
                "background": self.background,
                "monitored": self.monitored,
                "exit_code": self.exit_code,
                "started_ms": self.started_ms,
                "finished_ms": self.finished_ms,
                "duration_ms": self.duration_ms,
                "text": self.text,
                "trace": self.trace,
                "trace_line": self.trace_line,
                "wake_pattern": self.wake_pattern,
                "wake_silence_s": self.wake_silence_s,
                "session_id": self.session_id,
                "project_id": self.project_id,
            })),
        );
        node.temporal.insert("state".to_owned(), json!(self.state));
        let mut edges = vec![graph::edge(&me, "ON", &graph::machine())];
        let mut soft = Vec::new();
        if !self.session_id.is_empty() {
            soft.push(graph::edge(&me, "FOR", &graph::gref("Session", &self.session_id)));
        }
        if !self.project_id.is_empty() {
            soft.push(graph::edge(&me, "IN", &graph::gref("Project", &self.project_id)));
        }
        edges.shrink_to_fit();
        graph::Batch::observed(vec![node], edges, soft)
    }
}

/// The delegated commands of this plugin instance: kept in memory while they matter, every
/// change written through to the graph.
pub struct Commands {
    metas: Mutex<HashMap<String, CommandMeta>>,
    graph: std::sync::Arc<crate::graph::Graph>,
}

impl Commands {
    pub fn new(graph: std::sync::Arc<crate::graph::Graph>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Commands { metas: Mutex::new(HashMap::new()), graph })
    }

    fn keep(&self, meta: &CommandMeta) {
        if meta.id.is_empty() {
            return;
        }
        self.graph.put(meta.batch());
        lock(&self.metas).insert(meta.id.clone(), meta.clone());
    }

    pub fn get(&self, id: &str) -> Option<CommandMeta> {
        lock(&self.metas).get(id).cloned()
    }

    /// Commands a predecessor of this plugin left unfinished: held again, and asked how they ended.
    pub fn adopt(self: &std::sync::Arc<Self>, host: std::sync::Arc<dyn Host>) {
        let commands = self.clone();
        tokio::spawn(async move {
            let rows = commands
                .graph
                // `state` is temporal: a whole node leaves it out, so it is its own column.
                .query_all("MATCH (c:Command) WHERE c.finished_ms IS NULL AND c.key > $after RETURN c, c.state AS state, c.key AS k ORDER BY k LIMIT 5000", json!({}), 60)
                .await;
            let rows = match rows {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(error = %error.message, "the delegated commands could not be read from the graph");
                    return;
                }
            };
            for row in rows {
                let Some(props) = row.first().and_then(crate::graph::properties) else { continue };
                let text = |name: &str| crate::graph::text(props, name);
                let state = row.get(1).and_then(Value::as_str).unwrap_or("running").to_owned();
                if !matches!(state.as_str(), "running" | "waiting") {
                    continue;
                }
                let meta = CommandMeta {
                    id: text("key"),
                    provider: text("provider"),
                    provider_name: text("provider_name"),
                    command: text("command"),
                    cwd: text("cwd"),
                    purpose: text("purpose"),
                    background: props.get("background").and_then(Value::as_bool).unwrap_or(false),
                    monitored: props.get("monitored").and_then(Value::as_bool).unwrap_or(false),
                    state,
                    started_ms: crate::graph::number(props, "started_ms"),
                    text: text("text"),
                    trace: text("trace"),
                    trace_line: text("trace_line"),
                    wake_pattern: props.get("wake_pattern").and_then(Value::as_str).map(str::to_owned),
                    wake_silence_s: props.get("wake_silence_s").and_then(Value::as_u64),
                    session_id: text("session_id"),
                    project_id: text("project_id"),
                    ..CommandMeta::default()
                };
                if !meta.id.is_empty() {
                    lock(&commands.metas).insert(meta.id.clone(), meta);
                }
            }
            commands.resolve(host.as_ref()).await;
        });
    }

    /// Ask CortexTerm how each unfinished command is now: one that ended while this app did not
    /// hear it (CortexTerm restarted: `lost`, or this plugin was replaced) is taken as its end event.
    pub async fn resolve(&self, host: &dyn Host) {
        let unfinished: Vec<CommandMeta> = lock(&self.metas).values().filter(|m| matches!(m.state.as_str(), "running" | "waiting")).cloned().collect();
        if unfinished.is_empty() {
            return;
        }
        let (Some(link), Some(provider)) = (host.link(), provider(host).await) else { return };
        for meta in unfinished {
            let asker = Asker { session_id: meta.session_id.clone(), by: "agent".into() };
            let Ok(answer) = client::read(&link, &provider, "terminal.command.jobs.get", json!({"id": meta.id}), &asker, READ).await else { continue };
            let Ok(job) = serde_json::from_value::<Job>(answer) else { continue };
            if matches!(job.state.as_str(), "running" | "waiting") {
                continue;
            }
            let ended = CommandEnded { id: meta.id.clone(), node: String::new(), session_id: meta.session_id.clone(), job };
            let publish = |topic: &str, data: Value| host.publish(topic, data);
            on_event(self, &publish, cc_protocol::contracts::service::topics::COMMAND_ENDED, &serde_json::to_value(&ended).unwrap_or(Value::Null));
        }
    }

    /// The running background commands of a session (all sessions when None).
    pub fn running(&self, session: Option<&str>) -> Vec<CommandMeta> {
        let mut out: Vec<CommandMeta> = lock(&self.metas).values().filter(|m| m.background && matches!(m.state.as_str(), "running" | "waiting")).filter(|m| session.is_none_or(|s| m.session_id == s)).cloned().collect();
        out.sort_by_key(|m| m.started_ms);
        out
    }
}

/// What the session asks to run, prepared by `run` (its checks and approvals already passed).
pub struct Request<'a> {
    pub session_id: &'a str,
    pub project_id: &'a str,
    pub command: &'a str,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Folders in front of PATH: cx, the toolchains, the venv.
    pub path: Vec<String>,
    pub sandboxed: bool,
    pub writable: Vec<String>,
    /// check | build | server | shell
    pub purpose: &'a str,
    pub timeout_ms: u64,
    /// The person approved this command in this app's window (after a refusal).
    pub approved: bool,
    pub background: bool,
    pub watch: Option<Watch>,
    pub spill_dir: PathBuf,
}

impl Request<'_> {
    fn params(&self) -> Value {
        let env: Map<String, Value> = self.env.iter().map(|(key, value)| (key.clone(), Value::String(value.clone()))).collect();
        let mut params = json!({
            "command": self.command,
            "cwd": self.cwd.to_string_lossy(),
            "env": env,
            "path": self.path,
            "shell": "bash",
            "timeout_ms": self.timeout_ms,
            "monitor": self.purpose != "shell",
            "sandboxed": self.sandboxed,
            "writable": self.writable,
            "purpose": self.purpose,
            "background": self.background,
            "approved": self.approved,
        });
        if let Some(watch) = &self.watch {
            params["watch"] = serde_json::to_value(watch).unwrap_or(Value::Null);
        }
        params
    }

    fn meta(&self, provider: &Provider) -> CommandMeta {
        CommandMeta {
            provider: provider.node.clone(),
            provider_name: provider.name.clone(),
            command: self.command.to_owned(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            purpose: self.purpose.to_owned(),
            background: self.background,
            monitored: self.purpose != "shell",
            state: "running".into(),
            started_ms: util::now_ms(),
            wake_pattern: self.watch.as_ref().and_then(|w| w.pattern.clone()),
            wake_silence_s: self.watch.as_ref().and_then(|w| w.silence_s),
            session_id: self.session_id.to_owned(),
            project_id: self.project_id.to_owned(),
            ..CommandMeta::default()
        }
    }
}

/// What came of handing a command to CortexTerm.
pub enum Outcome {
    /// It ran (or stopped): its output as from a shell, and the line for the result.
    Ran { run: RunOutput, line: String },
    /// A background run started.
    Started { line: String },
    /// CortexTerm's risk check wants the person's approval first.
    Refused { risk: String, reason: String },
    /// No CortexTerm now, or it is restarting: run it here.
    Elsewhere,
    /// The call failed in a way running it here would not fix.
    Failed(String),
}

/// `[cmd://run-3f2a: done, exit 0 in 42 s; peak CPU 380%, 1.7 GB; ran cargo, rustc ×4. Read cmd://run-3f2a for the output, the machine's numbers, or what was happening at a moment]`
fn result_line(meta: &CommandMeta) -> String {
    let mut first = format!("{} in {}", meta.state, text::duration(meta.duration_ms));
    if !meta.trace_line.is_empty() {
        // The provider's line starts with its own `trace://<id>:`; the numbers are what counts.
        let numbers = meta.trace_line.split_once(": ").map(|(_, rest)| rest).unwrap_or(&meta.trace_line);
        first.push_str("; ");
        first.push_str(numbers);
    }
    let more = if meta.monitored { "the output, the machine's numbers, or what was happening at a moment" } else { "its output" };
    format!("[cmd://{}: ran in CortexTerm, {first}. Read cmd://{} for {more}]", meta.id, meta.id)
}

/// Run a foreground command in CortexTerm, its output streamed to `on_output` as it comes.
pub async fn run(commands: &Commands, host: &dyn Host, provider: &Provider, request: &Request<'_>, cancel: CancellationToken, mut on_output: impl FnMut(&[u8]) + Send) -> Outcome {
    let Some(link) = host.link() else { return Outcome::Elsewhere };
    let asker = Asker { session_id: request.session_id.to_owned(), by: "agent".into() };
    let mut meta = request.meta(provider);
    let (items_tx, mut items_rx) = mpsc::unbounded_channel::<Value>();
    let call = client::run(&link, provider, "terminal.command.run", request.params(), &asker, items_tx, cancel.clone());
    tokio::pin!(call);
    let mut capture = Capture::new(&request.spill_dir);
    let started = Instant::now();
    let ran = loop {
        tokio::select! {
            ran = &mut call => break ran,
            Some(item) = items_rx.recv() => match item.get("kind").and_then(Value::as_str) {
                // The provider names the job first, so a run that is cut off can be found again.
                Some("job") => {
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        meta.id = id.to_owned();
                        meta.trace = item.get("trace").and_then(Value::as_str).unwrap_or("").trim_start_matches("trace://").to_owned();
                        commands.keep(&meta);
                    }
                }
                _ => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        capture.push(text.as_bytes());
                        on_output(text.as_bytes());
                    }
                }
            },
        }
    };
    while let Ok(item) = items_rx.try_recv() {
        if item.get("kind").and_then(Value::as_str) != Some("job")
            && let Some(text) = item.get("text").and_then(Value::as_str)
        {
            capture.push(text.as_bytes());
            on_output(text.as_bytes());
        }
    }
    match ran {
        Ran::Job(job) if job.state == "refused" => {
            let (risk, reason) = job.needs_approval.map(|why| (why.risk, why.reason)).unwrap_or_else(|| ("unknown".into(), "CortexTerm's risk check stopped it".into()));
            Outcome::Refused { risk, reason }
        }
        Ran::Job(job) => {
            meta.take(&job);
            commands.keep(&meta);
            let cancelled = cancel.is_cancelled();
            let timed_out = job.state == "stopped" && !cancelled && job.duration_ms + 1_000 >= request.timeout_ms;
            let stopped = match job.state.as_str() {
                "lost" => Some("[CortexTerm stopped while this ran; its result is unknown]".to_owned()),
                "stopped" if !cancelled && !timed_out => Some(format!("[stopped in CortexTerm: {}]", job.tail.last().map(String::as_str).unwrap_or("someone stopped it"))),
                _ => None,
            };
            let run = RunOutput {
                exit_code: job.exit_code,
                timed_out,
                cancelled,
                shell_exited: false,
                output: capture.finish(),
                duration_ms: if job.duration_ms > 0 { job.duration_ms } else { started.elapsed().as_millis() as u64 },
                pipe_status: Vec::new(),
                stopped,
                running: None,
            };
            Outcome::Ran { run, line: result_line(&meta) }
        }
        Ran::Elsewhere(error) => {
            forget();
            tracing::info!(error = %error.message, "CortexTerm cannot run commands now; running here");
            Outcome::Elsewhere
        }
        Ran::Failed(error) if meta.id.is_empty() => Outcome::Failed(format!("CortexTerm could not run it: {}", error.message)),
        Ran::Failed(error) => {
            // The run had started: it never runs twice. CortexTerm reports it `lost` when it is back.
            meta.state = "lost".into();
            meta.text = format!("CortexTerm stopped answering while this ran ({}); its result is unknown.", error.message);
            meta.duration_ms = started.elapsed().as_millis() as u64;
            commands.keep(&meta);
            let run = RunOutput {
                exit_code: None,
                timed_out: false,
                cancelled: cancel.is_cancelled(),
                shell_exited: false,
                output: capture.finish(),
                duration_ms: meta.duration_ms,
                pipe_status: Vec::new(),
                stopped: Some(format!("[CortexTerm stopped answering while this ran; its result is unknown. Read cmd://{} later: it says how it ended]", meta.id)),
                running: None,
            };
            Outcome::Ran { run, line: String::new() }
        }
    }
}

/// Start a background command in CortexTerm.
pub async fn start(commands: &Commands, host: &dyn Host, provider: &Provider, request: &Request<'_>) -> Outcome {
    let Some(link) = host.link() else { return Outcome::Elsewhere };
    let asker = Asker { session_id: request.session_id.to_owned(), by: "agent".into() };
    let mut meta = request.meta(provider);
    let (items_tx, _items_rx) = mpsc::unbounded_channel::<Value>();
    match client::run(&link, provider, "terminal.command.run", request.params(), &asker, items_tx, CancellationToken::new()).await {
        Ran::Job(job) if job.state == "refused" => {
            let (risk, reason) = job.needs_approval.map(|why| (why.risk, why.reason)).unwrap_or_else(|| ("unknown".into(), "CortexTerm's risk check stopped it".into()));
            Outcome::Refused { risk, reason }
        }
        Ran::Job(job) => {
            meta.take(&job);
            commands.keep(&meta);
            let mut when = vec!["it exits".to_owned()];
            if let Some(pattern) = &meta.wake_pattern {
                when.push(format!("a line of its output matches /{pattern}/"));
            }
            if let Some(silence) = meta.wake_silence_s {
                when.push(format!("it prints nothing for {silence} s"));
            }
            let when = when.join(", or when ");
            let line = format!(
                "Started background job {id} in CortexTerm. You are woken when {when}. Its output so far, its state and the machine's numbers: read cmd://{id}. Stop it: `cx job kill {id}`.\n",
                id = meta.id
            );
            Outcome::Started { line }
        }
        Ran::Elsewhere(_) => {
            forget();
            Outcome::Elsewhere
        }
        Ran::Failed(error) => Outcome::Failed(format!("CortexTerm could not start it: {}", error.message)),
    }
}

/// Stop a delegated command (`cx job kill <id>`); None when `id` is not one.
pub async fn stop(commands: &Commands, host: &dyn Host, id: &str) -> Option<Result<String, String>> {
    let mut meta = commands.get(id).or_else(|| id.starts_with("run-").then(CommandMeta::default))?;
    let provider = provider(host).await;
    let (Some(link), Some(provider)) = (host.link(), provider) else {
        return Some(Err("CortexTerm is not running now, so its job cannot be stopped from here".into()));
    };
    let asker = Asker { session_id: meta.session_id.clone(), by: "agent".into() };
    let (items_tx, _items_rx) = mpsc::unbounded_channel::<Value>();
    Some(match client::run(&link, &provider, "terminal.command.stop", json!({"id": id}), &asker, items_tx, CancellationToken::new()).await {
        Ran::Job(job) => {
            if meta.id.is_empty() {
                meta.id = id.to_owned();
            }
            meta.take(&job);
            commands.keep(&meta);
            Ok(text::job(&job))
        }
        Ran::Elsewhere(error) | Ran::Failed(error) => Err(error.message),
    })
}

/// A provider's event: a background job ended or matched its watch. The session hears it as a
/// job event of this plugin (`tools.job`), the same as one of its own background jobs.
pub fn on_event(commands: &Commands, publish: &dyn Fn(&str, Value), topic: &str, data: &Value) {
    use cc_protocol::contracts::service::topics;
    use cc_protocol::contracts::tools::topics::JOB_EVENT;
    if topic == topics::COMMAND_ENDED
        && let Ok(ended) = serde_json::from_value::<CommandEnded>(data.clone())
    {
        let mut meta = commands.get(&ended.id).unwrap_or_else(|| CommandMeta { id: ended.id.clone(), session_id: ended.session_id.clone(), ..CommandMeta::default() });
        let was_running = matches!(meta.state.as_str(), "running" | "waiting" | "");
        meta.take(&ended.job);
        commands.keep(&meta);
        // A foreground run's end was already its tool result.
        if !meta.background || !was_running {
            return;
        }
        publish(
            JOB_EVENT,
            json!({
                "job_id": meta.id,
                "session_id": meta.session_id,
                "project_id": meta.project_id,
                // In the words of this plugin's own jobs.
                "state": match meta.state.as_str() { "done" | "failed" => "exited", "stopped" => "killed", other => other },
                "exit_code": meta.exit_code,
                "reason": "exit",
                "artifact": format!("cmd://{}", meta.id),
                "output_tail": format!("{}\n{}", ended.job.tail.join("\n"), meta.text),
            }),
        );
    } else if topic == topics::COMMAND_MATCHED
        && let Ok(matched) = serde_json::from_value::<CommandMatched>(data.clone())
    {
        let meta = commands.get(&matched.id).unwrap_or_else(|| CommandMeta { id: matched.id.clone(), session_id: matched.session_id.clone(), ..CommandMeta::default() });
        let mut event = json!({
            "job_id": meta.id,
            "session_id": meta.session_id,
            "project_id": meta.project_id,
            "state": "running",
            "reason": if matched.why == "silence" { "silence" } else { "match" },
            "artifact": format!("cmd://{}", meta.id),
            "output_tail": matched.line,
        });
        if matched.why == "silence" {
            event["silent_s"] = json!(meta.wake_silence_s.unwrap_or(0));
        } else {
            event["matched"] = json!(matched.line);
        }
        publish(JOB_EVENT, event);
    }
}

/// `cmd://<id>` (status and summary), `cmd://<id>/output` (lines: `?from=-200&count=200&filter=error`),
/// `cmd://<id>?at=14:02:10` (what was happening then): what CortexTerm keeps of a command.
pub async fn read(commands: &Commands, host: &dyn Host, path: &str, params: &HashMap<String, String>, workspace: &Path) -> Result<String, UriError> {
    let path = path.trim_matches('/');
    let (id, part) = path.split_once('/').unwrap_or((path, ""));
    if id.is_empty() {
        return Err(UriError::new("bad_uri", "cmd:// needs the command's id: cmd://run-…"));
    }
    let known = commands.get(id);
    let (Some(link), Some(provider)) = (host.link(), provider(host).await) else {
        // CortexTerm is not running: what this app kept is all there is.
        return match known {
            Some(meta) if !meta.text.is_empty() => Ok(format!("{}\n(CortexTerm is not running now; its output and numbers are kept there. This is what it answered then.)", meta.text)),
            _ => Err(UriError::new("unavailable", format!("cmd://{id}: CortexTerm is not running now; it keeps the command's output and numbers"))),
        };
    };
    let asker = Asker { session_id: known.as_ref().map(|m| m.session_id.clone()).unwrap_or_default(), by: "agent".into() };
    let failed = |error: cc_protocol::RpcError| match error.code.as_str() {
        cc_protocol::codes::NOT_FOUND => UriError::new("not_found", format!("cmd://{id}: CortexTerm no longer has it ({})", error.message)),
        _ => UriError::new("unavailable", format!("cmd://{id}: {}", error.message)),
    };
    let root = Some(workspace);
    if part == "output" {
        let mut ask = json!({"run_id": id});
        if let Some(from) = params.get("from").and_then(|v| v.parse::<i64>().ok()) {
            ask["from"] = json!(from);
        } else {
            ask["from"] = json!(-200);
        }
        if let Some(count) = params.get("count").and_then(|v| v.parse::<u64>().ok()) {
            ask["count"] = json!(count);
        }
        if let Some(filter) = params.get("filter") {
            ask["filter"] = json!(filter);
        }
        let answer = client::read(&link, &provider, "terminal.command.output.read", ask, &asker, READ).await.map_err(failed)?;
        let lines: Vec<String> = answer["lines"].as_array().into_iter().flatten().filter_map(|line| line.as_str().map(str::to_owned).or_else(|| line["text"].as_str().map(str::to_owned))).collect();
        if lines.is_empty() {
            return Ok(format!("cmd://{id}: no output lines in that range"));
        }
        return Ok(util::relative_paths(&lines.join("\n"), workspace));
    }
    let mut ask = json!({"id": id, "size": "summary"});
    if let Some(at) = params.get("at") {
        let Some(at_ms) = moment_ms(at, known.as_ref().map(|m| m.started_ms).unwrap_or(0)) else {
            return Err(UriError::new("bad_uri", format!("cmd://{id}?at=…: give a time as HH:MM:SS (today, local) or Unix milliseconds")));
        };
        ask = json!({"id": id, "at": at_ms});
    }
    let answer = client::read(&link, &provider, "terminal.command.jobs.get", ask, &asker, READ).await.map_err(failed)?;
    let job: Job = serde_json::from_value(answer).map_err(|error| UriError::new("uri_error", format!("cmd://{id}: an unexpected answer ({error})")))?;
    if let Some(mut meta) = known {
        meta.take(&job);
        commands.keep(&meta);
    }
    let mut out = text::job(&job);
    match &job.detail {
        Some(detail) if params.contains_key("at") => {
            if let Ok(moment) = serde_json::from_value::<Moment>(detail.clone()) {
                out.push_str("\n\n");
                out.push_str(&text::moment(&moment, root));
            }
        }
        Some(detail) => {
            let summary = text::summary(detail, root);
            if !summary.is_empty() {
                out.push_str("\n\n");
                out.push_str(&summary);
            }
        }
        None => {}
    }
    out.push_str(&format!("\n\nMore: cmd://{id}/output for its output lines (?filter=error, ?from=-200)"));
    if job.trace.is_some() {
        out.push_str(&format!(", cmd://{id}?at=HH:MM:SS for what ran, the memory and the open files at that moment"));
    }
    Ok(util::relative_paths(&out, workspace))
}

/// `14:02:10` (today, local time; the day the run started when given) or Unix milliseconds.
fn moment_ms(text: &str, started_ms: u64) -> Option<u64> {
    if let Ok(ms) = text.parse::<u64>() {
        return Some(ms);
    }
    let mut parts = text.split(':').map(|part| part.parse::<u64>().ok());
    let (hours, minutes) = (parts.next()??, parts.next()??);
    let seconds = parts.next().flatten().unwrap_or(0);
    let base = if started_ms > 0 { started_ms } else { util::now_ms() };
    // Local midnight of that day: the offset from UTC as libc knows it.
    let secs = (base / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes only into `tm`.
    unsafe { libc::localtime_r(&secs, &mut tm) };
    let into_day = (tm.tm_hour as u64 * 3600 + tm.tm_min as u64 * 60 + tm.tm_sec as u64) * 1000;
    let midnight = base - into_day - base % 1000;
    Some(midnight + (hours * 3600 + minutes * 60 + seconds) * 1000)
}

/// The variables a session's shell exported (`export -p`, saved after every command), for a
/// fresh shell: `declare -x NAME="value"` lines, the machine's own ones left out.
pub fn saved_exports(file: Option<&Path>) -> Vec<(String, String)> {
    let Some(text) = file.and_then(|file| std::fs::read_to_string(file).ok()) else { return Vec::new() };
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("declare -x ") else { continue };
        let Some((name, value)) = rest.split_once('=') else { continue };
        if NOT_SENT.contains(&name) || name.starts_with("BASH") || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }
        let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else { continue };
        let mut unquoted = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some(next @ ('"' | '\\' | '$' | '`')) => unquoted.push(next),
                    Some(other) => {
                        unquoted.push('\\');
                        unquoted.push(other);
                    }
                    None => unquoted.push('\\'),
                }
            } else {
                unquoted.push(c);
            }
        }
        out.push((name.to_owned(), unquoted));
    }
    out
}

/// Whether `command` calls a shell function the session defined earlier (`declare -f`, saved after
/// every command): a fresh shell would not have it, so such a command stays in the session's shell.
pub fn uses_session_function(file: Option<&Path>, command: &str) -> bool {
    let Some(text) = file.and_then(|file| std::fs::read_to_string(file).ok()) else { return false };
    let names: Vec<&str> = text.lines().filter_map(|line| line.strip_suffix(" () ")).filter(|name| !name.contains(' ')).collect();
    if names.is_empty() {
        return false;
    }
    command.split(|c: char| c.is_whitespace() || ";|&()`$".contains(c)).any(|word| names.contains(&word))
}
