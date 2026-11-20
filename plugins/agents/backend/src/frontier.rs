//! Frontier workers (docs/FRONTIER-AGENTS.md): an agent's task done by a frontier CLI, Claude Code
//! or Codex, run headless in the project's folder, instead of the chat flow's local agent loop.
//!
//! - **Who works.** The setting `agents.worker` chooses `local`, `claude` or `openai`; a frontier
//!   worker works only when its CLI is found and says it is signed in, else the local agents do
//!   (and the choice says why: [`Frontier::workers`]).
//! - **The CLI's hands are CortexCode's.** Its own tools are off (Claude: no built-in tools; Codex:
//!   a read-only sandbox). Its only way to change files is `cx mcp`, the tools plugin's model tools
//!   as an MCP server, held to the agent's session: the file reservations, the write sandbox, the
//!   step log and undo cover it as they cover a local agent.
//! - **Its run is the agent's run.** The task's text goes in as the first message; every message,
//!   thought, tool call and result goes into the agent session's journal as the chat flow writes
//!   them, so the agent's view and the lead's facts read the same. The run ends with a report in a
//!   JSON Schema (`status`, `summary`, `question`): `waiting` means it asked its lead (`ask_lead`)
//!   and the answer resumes the CLI's own session.
//! - **Steering.** A steer is a `steer` entry in the agent's journal. A running Claude session
//!   takes it as its next message when its current turn ends (stream-JSON input keeps the process
//!   open); a Codex turn is followed by a resumed turn carrying it.

use crate::data::Dataset;
use crate::host::Host;
use crate::journal::Journal;
use crate::scheduler::AgentKey;
use cc_protocol::contracts::agents::{WorkerInfo, WorkersResult, workers};
use cc_protocol::contracts::chatflow::AgentRunResult;
use cc_protocol::contracts::frame::{SessionStatus, kinds};
use cc_protocol::contracts::memory::{AgentRecord, Task};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::records::methods as records_methods;
use cc_protocol::contracts::tools::{AgentEnvResult, TimelineResult, methods as tools_methods};
use cc_protocol::{RpcError, codes};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The setting that chooses who does agent work.
pub const WORKER_SETTING: &str = "agents.worker";
/// The MCP server's name in the CLI's configuration; its tools are `mcp__cortex__<tool>` there.
const MCP_SERVER: &str = "cortex";
/// How long a detection (paths and sign-in) is reused before the CLIs are asked again.
const DETECT_FOR: Duration = Duration::from_secs(60);
/// Longest a sign-in check may take.
const STATUS_TIMEOUT: Duration = Duration::from_secs(15);
/// What of a CLI's error output is kept for the run's failure text.
const STDERR_TAIL: usize = 4000;

/// The report every frontier run ends with (strict: every field required, nothing else).
fn report_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["status", "summary", "question"],
        "properties": {
            "status": {"type": "string", "enum": ["done", "waiting", "blocked"]},
            "summary": {"type": "string"},
            "question": {"type": "string"},
        },
    })
}

/// A worker's defaults: its label, model and effort (the settings `agents.<worker>.model` and
/// `agents.<worker>.effort` override the last two).
fn defaults(worker: &str) -> (&'static str, &'static str, &'static str) {
    match worker {
        workers::CLAUDE => ("Claude", "claude-opus-5-5", "medium"),
        workers::OPENAI => ("OpenAI", "gpt-6.2-sol", "high"),
        _ => ("Local", "", ""),
    }
}

/// A worker's name as the user reads it.
pub fn label(worker: &str) -> &'static str {
    defaults(worker).0
}

/// How a run ended, for the agents plugin.
pub struct RunEnd {
    pub result: AgentRunResult,
    /// The CLI's own session id (kept on the agent's record, so the next run resumes it).
    pub external_session: Option<String>,
    /// The question to the lead, when the run ended `waiting`.
    pub question: Option<String>,
    /// The CLI failed before it did any work (it could not start, or refused the model): the
    /// task is the local model's now, and the worker cannot be used until it is looked at again.
    pub start_failed: bool,
    /// The CLI's own failure text, when it failed.
    pub error: Option<String>,
}

/// One CLI as detection found it.
#[derive(Clone, Default)]
struct Found {
    path: Option<PathBuf>,
    /// Why it cannot work (not installed, not signed in); None when it can.
    problem: Option<String>,
    /// What `<cli> --version` says.
    version: Option<String>,
}

#[derive(Default)]
struct Detected {
    at: Option<Instant>,
    found: HashMap<String, Found>,
}

/// Frontier workers: detection and the runs going now (for steering).
#[derive(Default)]
pub struct Frontier {
    detected: Mutex<Detected>,
    /// Per worker: why its last run failed before doing any work (e.g. a CLI too old for its
    /// model). Kept until the CLIs are looked at again fresh (the settings panel opening, a
    /// choice) or one of its runs works.
    failures: Mutex<HashMap<String, String>>,
    live: Mutex<HashMap<AgentKey, mpsc::UnboundedSender<()>>>,
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

async fn setting(host: &Host, path: &str) -> Option<String> {
    let value = host.call(records_methods::SETTINGS_GET, json!({"path": path})).await.ok()?;
    value.get("value").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned)
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Where a CLI is: on PATH, then the places its installers use (the app's PATH is often short).
fn find_cli(worker: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let (name, extra): (&str, Vec<PathBuf>) = match worker {
        workers::CLAUDE => (
            "claude",
            vec![
                PathBuf::from("/opt/homebrew/bin/claude"),
                PathBuf::from("/usr/local/bin/claude"),
                home.join(".local/bin/claude"),
                home.join(".claude/local/claude"),
                home.join(".npm-global/bin/claude"),
                home.join(".bun/bin/claude"),
            ],
        ),
        workers::OPENAI => (
            "codex",
            vec![
                PathBuf::from("/opt/homebrew/bin/codex"),
                PathBuf::from("/usr/local/bin/codex"),
                home.join(".local/bin/codex"),
                home.join(".npm-global/bin/codex"),
                // The Codex CLI the ChatGPT app ships.
                PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex"),
                PathBuf::from("/Applications/Codex.app/Contents/Resources/codex-cli/bin/codex"),
            ],
        ),
        _ => return None,
    };
    let on_path = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).map(|dir| dir.join(name)).collect::<Vec<_>>())
        .unwrap_or_default();
    on_path.into_iter().chain(extra).find(|path| executable(path))
}

/// What a CLI says about its sign-in: None when it is signed in, else the problem in plain words.
async fn sign_in_problem(worker: &str, path: &Path) -> Option<String> {
    let args: &[&str] = match worker {
        workers::CLAUDE => &["auth", "status", "--json"],
        _ => &["login", "status"],
    };
    let mut command = Command::new(path);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let output = match tokio::time::timeout(STATUS_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Some(format!("it cannot be started ({error})")),
        Err(_) => return Some("it did not answer whether it is signed in".to_owned()),
    };
    match worker {
        workers::CLAUDE => {
            // An API key in the environment signs Claude Code in as well.
            if std::env::var("ANTHROPIC_API_KEY").is_ok_and(|key| !key.is_empty()) {
                return None;
            }
            let status: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
            match status.get("loggedIn").and_then(Value::as_bool) {
                Some(true) => None,
                Some(false) => Some("it is not signed in (run `claude auth login`)".to_owned()),
                None => Some("it did not say whether it is signed in".to_owned()),
            }
        }
        _ => {
            if output.status.success() {
                None
            } else {
                Some("it is not signed in (run `codex login`)".to_owned())
            }
        }
    }
}

/// What `<cli> --version` says (its first line), when it answers.
async fn version_of(path: &Path) -> Option<String> {
    let mut command = Command::new(path);
    command.arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(STATUS_TIMEOUT, command.output()).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().next().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned)
}

impl Frontier {
    /// The CLIs found, their versions and sign-in, asked again when `fresh` or after
    /// [`DETECT_FOR`]. A fresh look also forgets the runs that could not start: the user may have
    /// updated or signed in since.
    async fn detect(&self, fresh: bool) -> HashMap<String, Found> {
        {
            let detected = lock(&self.detected);
            if !fresh && detected.at.is_some_and(|at| at.elapsed() < DETECT_FOR) {
                return detected.found.clone();
            }
        }
        if fresh {
            lock(&self.failures).clear();
        }
        let mut found = HashMap::new();
        for worker in [workers::CLAUDE, workers::OPENAI] {
            let path = find_cli(worker);
            let (problem, version) = match &path {
                Some(path) => (sign_in_problem(worker, path).await, version_of(path).await),
                None => (Some("it is not installed on this computer".to_owned()), None),
            };
            found.insert(worker.to_owned(), Found { path, problem, version });
        }
        let mut detected = lock(&self.detected);
        detected.at = Some(Instant::now());
        detected.found = found.clone();
        found
    }

    /// Who does agent work: the choice, who works now, and every worker this computer has.
    pub async fn workers(&self, host: &Host, fresh: bool) -> WorkersResult {
        let found = self.detect(fresh).await;
        let chosen = setting(host, WORKER_SETTING).await.filter(|worker| workers::ALL.contains(&worker.as_str())).unwrap_or_else(|| workers::LOCAL.to_owned());
        let mut list = vec![WorkerInfo { id: workers::LOCAL.to_owned(), label: "Local".to_owned(), installed: true, signed_in: true, ..WorkerInfo::default() }];
        for worker in [workers::CLAUDE, workers::OPENAI] {
            let (label, model, effort) = defaults(worker);
            let Found { path, problem, version } = found.get(worker).cloned().unwrap_or(Found { problem: Some("it was not looked for".to_owned()), ..Found::default() });
            let model = setting(host, &format!("agents.{worker}.model")).await.unwrap_or_else(|| model.to_owned());
            let effort = setting(host, &format!("agents.{worker}.effort")).await.unwrap_or_else(|| effort.to_owned());
            let signed_in = path.is_some() && problem.is_none();
            // A run that could not start says what stops it (in the CLI's own words).
            let failure = lock(&self.failures).get(worker).cloned();
            let problem = problem.or_else(|| failure.map(|failure| format!("its last run could not start: {failure}")));
            list.push(WorkerInfo {
                id: worker.to_owned(),
                label: label.to_owned(),
                installed: path.is_some(),
                signed_in,
                path: path.map(|path| path.to_string_lossy().into_owned()),
                version,
                model: Some(model),
                effort: Some(effort),
                problem,
            });
        }
        let chosen_info = list.iter().find(|info| info.id == chosen);
        let (active, fallback) = match chosen_info {
            Some(info) if info.id != workers::LOCAL && info.problem.is_some() => (
                workers::LOCAL.to_owned(),
                Some(format!("{} cannot work now: {}. The local model's agents do the work.", info.label, info.problem.clone().unwrap_or_default())),
            ),
            _ => (chosen.clone(), None),
        };
        WorkersResult { chosen, active, fallback, workers: list }
    }

    /// Choose who does agent work (the setting); a frontier worker must be able to work now.
    pub async fn set(&self, host: &Host, worker: &str) -> Result<WorkersResult, RpcError> {
        if !workers::ALL.contains(&worker) {
            return Err(RpcError::invalid_params(format!("`{worker}` is not a worker; choose one of {}", workers::ALL.join(", "))));
        }
        if workers::frontier(worker) {
            let listed = self.workers(host, true).await;
            if let Some(info) = listed.workers.iter().find(|info| info.id == worker)
                && let Some(problem) = &info.problem
            {
                return Err(RpcError::new(codes::UNAVAILABLE, format!("{} cannot be chosen: {problem}.", info.label)));
            }
        }
        host.call(records_methods::SETTINGS_SET, json!({"path": WORKER_SETTING, "value": worker, "by": "user", "description": "Who does agent work: local, claude or openai."})).await?;
        Ok(self.workers(host, false).await)
    }

    /// Tell a running frontier agent that a steer waits in its journal (false: none is running).
    pub fn nudge(&self, key: &AgentKey) -> bool {
        lock(&self.live).get(key).is_some_and(|sender| sender.send(()).is_ok())
    }

    /// One run of a frontier agent on its task, until it reports, fails, or is cancelled.
    pub async fn run(&self, host: &Host, data: &Dataset, record: &AgentRecord, task: &Task, root: &str, cancel: &CancellationToken) -> Result<RunEnd, RpcError> {
        let worker = record.worker.as_str();
        let listed = self.workers(host, false).await;
        let Some(info) = listed.workers.iter().find(|info| info.id == worker).cloned() else {
            return Err(RpcError::invalid_params(format!("`{worker}` is not a frontier worker")));
        };
        if let Some(problem) = &info.problem {
            return Err(RpcError::new(codes::UNAVAILABLE, format!("{} cannot work now: {problem}", info.label)));
        }
        let cli = PathBuf::from(info.path.clone().unwrap_or_default());
        let model = info.model.clone().unwrap_or_default();
        let effort = info.effort.clone().unwrap_or_default();
        let session_id = record.session_id.clone().ok_or_else(|| RpcError::internal(format!("agent {} has no session", record.name)))?;
        let access: AgentEnvResult = host.get(tools_methods::AGENT_ENV, json!({"session_id": session_id, "workspace": root})).await?;
        let mut env = access.env.clone();
        env.insert("CORTEX_AGENT".to_owned(), record.name.clone());

        let journal = Journal::load(host.clone(), &session_id).await?;
        let key = AgentKey { project_id: record.project_id.clone(), name: record.name.clone() };
        let (sender, nudges) = mpsc::unbounded_channel();
        lock(&self.live).insert(key.clone(), sender);
        let run = Run {
            host: host.clone(),
            data,
            task,
            root,
            session_id: session_id.clone(),
            run_id: format!("run-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            worker: worker.to_owned(),
            cli,
            model,
            effort,
            env,
            cx: access.cx.clone(),
            external: record.external_session.clone(),
            nudges,
            journal,
            step: 0,
            turns: 0,
            tool_calls: 0,
            usage: json!({}),
        };
        let outcome = run.go(cancel).await;
        lock(&self.live).remove(&key);
        if let Ok(end) = &outcome {
            if end.start_failed {
                lock(&self.failures).insert(worker.to_owned(), end.error.clone().unwrap_or_else(|| end.result.summary.clone()));
            } else if matches!(end.result.outcome.as_str(), "done" | "waiting" | "blocked") {
                lock(&self.failures).remove(worker);
            }
        }
        outcome
    }
}

/// One frontier run in progress.
struct Run<'a> {
    host: Host,
    data: &'a Dataset,
    task: &'a Task,
    root: &'a str,
    session_id: String,
    run_id: String,
    worker: String,
    cli: PathBuf,
    model: String,
    effort: String,
    env: BTreeMap<String, String>,
    cx: String,
    /// The CLI's own session (set once it said it).
    external: Option<String>,
    nudges: mpsc::UnboundedReceiver<()>,
    journal: Journal,
    step: u64,
    turns: u32,
    tool_calls: u32,
    usage: Value,
}

/// What one CLI turn ended with.
#[derive(Default)]
struct TurnEnd {
    /// The report's JSON (or text that should have been it).
    report: Option<Value>,
    /// The CLI's own failure, in its words.
    error: Option<String>,
}

impl Run<'_> {
    async fn go(mut self, cancel: &CancellationToken) -> Result<RunEnd, RpcError> {
        let started_ms = now_ms();
        let first = self.first_message().await?;
        self.journal
            .append(
                kinds::STATUS,
                json!({"state": "running", "run_id": self.run_id, "kind": "agent", "task_id": self.task.id, "worker": self.worker, "model": self.model, "effort": self.effort}),
            )
            .await?;
        let _ = self.host.session_status(&self.session_id, SessionStatus::Running).await;
        let ended = if self.worker == workers::CLAUDE { self.claude(first, cancel).await } else { self.codex(first, cancel).await };

        // The run's end is a turn's end for the tree: what its commands left behind goes, its file
        // reservations free, and what it changed is read from the step log.
        let _ = self.host.call(tools_methods::CLEANUP, json!({"workspace": self.root, "session_id": self.session_id, "since_ms": started_ms})).await;
        let changed_files = changed_since(&self.host, self.root, &self.session_id, started_ms).await;
        let raw_error = match &ended {
            Err(error) if error.code != codes::CANCELLED => Some(error.message.trim().to_owned()),
            Ok(turn) => turn.error.as_deref().map(str::trim).filter(|error| !error.is_empty()).map(str::to_owned),
            _ => None,
        };
        let (outcome, summary, question) = match ended {
            Err(error) if error.code == codes::CANCELLED => ("cancelled".to_owned(), self.data.text("frontier_cancelled", &[]), None),
            Err(error) => ("failed".to_owned(), self.data.text("frontier_failed", &[("error", &error.message)]), None),
            Ok(turn) => judge(self.data, &turn),
        };
        let result = AgentRunResult { outcome: outcome.clone(), summary: summary.clone(), turns: self.turns, tool_calls: self.tool_calls, changed_files, usage: self.usage.clone() };
        let _ = self
            .journal
            .append(kinds::STATUS, json!({"state": outcome, "detail": summary, "run_id": self.run_id, "task_id": self.task.id, "result": result, "worker": self.worker}))
            .await;
        let status = match outcome.as_str() {
            "done" => SessionStatus::Done,
            "cancelled" => SessionStatus::Cancelled,
            "waiting" | "blocked" => SessionStatus::Waiting,
            _ => SessionStatus::Failed,
        };
        let _ = self.host.session_status(&self.session_id, status).await;
        // Failed with no tool call made: the CLI never got to the work (it did not start, or it
        // refused its model or its sign-in), so nothing of the task was done by it.
        let start_failed = result.outcome == "failed" && self.tool_calls == 0;
        Ok(RunEnd { result, external_session: self.external.clone(), question, start_failed, error: raw_error })
    }

    /// The run's first message: the task on its first run; else what waits for it (the lead's
    /// answers and steers), or the follow-up that continues the task.
    async fn first_message(&mut self) -> Result<String, RpcError> {
        let first_time = !self.journal.entries().any(|entry| entry.kind == kinds::USER_MESSAGE);
        let mut parts = Vec::new();
        if first_time {
            parts.push(task_text(self.data, self.task));
        }
        parts.extend(self.take_steers().await?);
        if parts.is_empty() {
            parts.push(self.data.text("restart_followup", &[]));
        }
        let text = parts.join("\n\n");
        self.journal.append(kinds::USER_MESSAGE, json!({"text": text, "run_id": self.run_id, "task_id": self.task.id})).await?;
        Ok(text)
    }

    /// The steers waiting in the journal, consumed: their texts in order.
    async fn take_steers(&mut self) -> Result<Vec<String>, RpcError> {
        self.journal.sync().await?;
        let mut waiting: Vec<_> = self.journal.pending_steers("steer");
        waiting.extend(self.journal.pending_steers("followup"));
        waiting.sort_by_key(|entry| entry.seq);
        let mut texts = Vec::new();
        for entry in waiting {
            self.journal.consume_steer(entry.seq).await?;
            let text = str_field(&entry.data, "text").trim().to_owned();
            if !text.is_empty() {
                texts.push(text);
            }
        }
        Ok(texts)
    }

    /// The next message after a turn: steers that came meanwhile, journaled (None: nothing waits).
    async fn next_message(&mut self) -> Result<Option<String>, RpcError> {
        while self.nudges.try_recv().is_ok() {}
        let texts = self.take_steers().await?;
        if texts.is_empty() {
            return Ok(None);
        }
        let text = texts.join("\n\n");
        self.journal.append(kinds::USER_MESSAGE, json!({"text": text, "run_id": self.run_id, "task_id": self.task.id})).await?;
        Ok(Some(text))
    }

    // ---- Claude Code ---------------------------------------------------------------------------

    async fn claude(&mut self, first: String, cancel: &CancellationToken) -> Result<TurnEnd, RpcError> {
        let mut servers = serde_json::Map::new();
        servers.insert(MCP_SERVER.to_owned(), json!({"type": "stdio", "command": self.cx, "args": ["mcp"], "env": self.env}));
        let mcp = json!({"mcpServers": servers});
        let mut command = Command::new(&self.cli);
        command
            .args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"])
            .arg("--model")
            .arg(&self.model)
            .arg("--effort")
            .arg(&self.effort)
            // No built-in tools: CortexCode's tools over MCP are its only hands.
            .args(["--tools", ""])
            .args(["--strict-mcp-config", "--mcp-config"])
            .arg(mcp.to_string())
            .args(["--permission-mode", "dontAsk", "--allowedTools"])
            .arg(format!("mcp__{MCP_SERVER}"))
            .arg("--disable-slash-commands")
            .arg("--append-system-prompt")
            .arg(self.data.prompt("worker"))
            .arg("--json-schema")
            .arg(report_schema().to_string());
        let resumed = self.external.clone();
        match resumed {
            Some(id) => {
                command.arg("--resume").arg(id);
            }
            // The id is kept only once the CLI says it (its `system` event): a run that died before
            // its session was saved starts a new one next time instead of resuming nothing.
            None => {
                command.arg("--session-id").arg(uuid::Uuid::new_v4().to_string());
            }
        }
        let mut child = self.spawn(command)?;
        let mut stdin = child.stdin.take().ok_or_else(|| RpcError::internal("the CLI has no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| RpcError::internal("the CLI has no stdout"))?;
        let stderr = tail_of(child.stderr.take());
        send_user(&mut stdin, &first).await?;
        let mut lines = BufReader::new(stdout).lines();
        let mut stdin = Some(stdin);
        let mut last = TurnEnd::default();
        loop {
            let line = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = child.start_kill();
                    return Err(RpcError::cancelled());
                }
                line = lines.next_line() => line,
            };
            let line = match line {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => return Err(RpcError::internal(format!("reading the CLI's output failed: {error}"))),
            };
            let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
            match str_field(&event, "type") {
                "system" => {
                    if let Some(id) = event.get("session_id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                        self.external = Some(id.to_owned());
                    }
                }
                "assistant" => self.claude_assistant(&event).await?,
                "user" => self.claude_results(&event).await?,
                "result" => {
                    self.turns += 1;
                    if let Some(usage) = event.get("usage") {
                        self.usage = usage.clone();
                    }
                    last = claude_turn_end(&event);
                    // The lead's answers and steers that came during the turn are its next message;
                    // with none, the input ends and the CLI finishes.
                    match self.next_message().await? {
                        Some(text) if last.error.is_none() => {
                            if let Some(input) = stdin.as_mut() {
                                send_user(input, &text).await?;
                            }
                        }
                        _ => {
                            stdin.take();
                        }
                    }
                }
                _ => {}
            }
        }
        let status = child.wait().await.map_err(|error| RpcError::internal(format!("the CLI did not end cleanly: {error}")))?;
        if last.report.is_none() && last.error.is_none() && !status.success() {
            last.error = Some(stderr.await.unwrap_or_default());
        }
        Ok(last)
    }

    async fn claude_assistant(&mut self, event: &Value) -> Result<(), RpcError> {
        for part in event.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
            match str_field(part, "type") {
                "text" => {
                    let text = str_field(part, "text");
                    if !text.trim().is_empty() {
                        self.journal.append(kinds::ASSISTANT_MESSAGE, json!({"text": text, "run_id": self.run_id})).await?;
                    }
                }
                "thinking" => {
                    let text = str_field(part, "thinking");
                    if !text.trim().is_empty() {
                        self.journal.append(kinds::REASONING, json!({"text": text, "run_id": self.run_id})).await?;
                    }
                }
                "tool_use" => {
                    let name = str_field(part, "name");
                    // The structured report is not a step of the work.
                    if name == "StructuredOutput" {
                        continue;
                    }
                    let input = part.get("input").cloned().unwrap_or_else(|| json!({}));
                    self.tool_call(str_field(part, "id"), name, &input).await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn claude_results(&mut self, event: &Value) -> Result<(), RpcError> {
        for part in event.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
            if str_field(part, "type") != "tool_result" {
                continue;
            }
            let ok = !part.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let output = content_text(part.get("content"));
            self.tool_result(str_field(part, "tool_use_id"), ok, &output).await?;
        }
        Ok(())
    }

    // ---- Codex ---------------------------------------------------------------------------------

    async fn codex(&mut self, first: String, cancel: &CancellationToken) -> Result<TurnEnd, RpcError> {
        // Codex reads the report's schema from a file: one per run, removed when the run ends.
        let schema_file = std::env::temp_dir().join(format!("cortex-agent-report-{}.json", self.run_id));
        std::fs::write(&schema_file, report_schema().to_string()).map_err(|error| RpcError::internal(format!("cannot write the report schema: {error}")))?;
        let mut message = first;
        let ended = loop {
            let turn = self.codex_turn(&message, &schema_file, cancel).await;
            match turn {
                Ok(turn) if turn.error.is_none() => match self.next_message().await {
                    Ok(Some(text)) => message = text,
                    Ok(None) => break Ok(turn),
                    Err(error) => break Err(error),
                },
                other => break other,
            }
        };
        let _ = std::fs::remove_file(&schema_file);
        ended
    }

    async fn codex_turn(&mut self, message: &str, schema_file: &Path, cancel: &CancellationToken) -> Result<TurnEnd, RpcError> {
        let env_table = format!(
            "{{{}}}",
            self.env.iter().map(|(key, value)| format!("{key} = {}", toml_string(value.as_str()))).collect::<Vec<_>>().join(", ")
        );
        let mut command = Command::new(&self.cli);
        command.arg("exec");
        if let Some(id) = self.external.clone() {
            command.arg("resume").arg(id);
        }
        command
            .args(["--json", "--skip-git-repo-check", "-m"])
            .arg(&self.model)
            .arg("--output-schema")
            .arg(schema_file)
            // Its own shell may look but not change: CortexCode's tools over MCP are its hands.
            .args(["-c", "sandbox_mode=\"read-only\"", "-c", "approval_policy=\"never\""])
            .arg("-c")
            .arg(format!("model_reasoning_effort={}", toml_string(self.effort.as_str())))
            .arg("-c")
            .arg(format!("developer_instructions={}", toml_string(self.data.prompt("worker"))))
            .arg("-c")
            .arg(format!("mcp_servers.{MCP_SERVER}.command={}", toml_string(self.cx.as_str())))
            .arg("-c")
            .arg(format!("mcp_servers.{MCP_SERVER}.args=[\"mcp\"]"))
            .arg("-c")
            .arg(format!("mcp_servers.{MCP_SERVER}.env={env_table}"))
            // The message comes on stdin.
            .arg("-");
        let mut child = self.spawn(command)?;
        let mut stdin = child.stdin.take().ok_or_else(|| RpcError::internal("the CLI has no stdin"))?;
        stdin.write_all(message.as_bytes()).await.map_err(|error| RpcError::internal(format!("cannot send the task to the CLI: {error}")))?;
        drop(stdin);
        let stdout = child.stdout.take().ok_or_else(|| RpcError::internal("the CLI has no stdout"))?;
        let stderr = tail_of(child.stderr.take());
        let mut lines = BufReader::new(stdout).lines();
        let mut end = TurnEnd::default();
        let mut last_message: Option<String> = None;
        loop {
            let line = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = child.start_kill();
                    return Err(RpcError::cancelled());
                }
                line = lines.next_line() => line,
            };
            let line = match line {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => return Err(RpcError::internal(format!("reading the CLI's output failed: {error}"))),
            };
            let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
            match str_field(&event, "type") {
                "thread.started" => {
                    if let Some(id) = event.get("thread_id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                        self.external = Some(id.to_owned());
                    }
                }
                "item.started" => {
                    let item = event.get("item").cloned().unwrap_or(Value::Null);
                    if str_field(&item, "type") == "mcp_tool_call" {
                        let arguments = item.get("arguments").cloned().unwrap_or_else(|| json!({}));
                        self.tool_call(str_field(&item, "id"), &format!("mcp__{}__{}", str_field(&item, "server"), str_field(&item, "tool")), &arguments).await?;
                    }
                }
                "item.completed" => {
                    let item = event.get("item").cloned().unwrap_or(Value::Null);
                    match str_field(&item, "type") {
                        "agent_message" => {
                            let text = str_field(&item, "text").to_owned();
                            if !text.trim().is_empty() {
                                self.journal.append(kinds::ASSISTANT_MESSAGE, json!({"text": text, "run_id": self.run_id})).await?;
                                last_message = Some(text);
                            }
                        }
                        "reasoning" => {
                            let text = str_field(&item, "text");
                            if !text.trim().is_empty() {
                                self.journal.append(kinds::REASONING, json!({"text": text, "run_id": self.run_id})).await?;
                            }
                        }
                        "mcp_tool_call" => {
                            let failed = item.get("error").is_some_and(|error| !error.is_null()) || str_field(&item, "status") == "failed";
                            let output = match item.get("result") {
                                Some(result) if !result.is_null() => content_text(result.get("content")),
                                _ => item.pointer("/error/message").and_then(Value::as_str).unwrap_or_default().to_owned(),
                            };
                            self.tool_result(str_field(&item, "id"), !failed, &output).await?;
                        }
                        _ => {}
                    }
                }
                "turn.completed" => {
                    self.turns += 1;
                    if let Some(usage) = event.get("usage") {
                        self.usage = usage.clone();
                    }
                }
                "turn.failed" => {
                    end.error = Some(event.pointer("/error/message").and_then(Value::as_str).unwrap_or("the turn failed").to_owned());
                }
                "error" => {
                    end.error = Some(str_field(&event, "message").to_owned()).filter(|text| !text.is_empty()).or(end.error);
                }
                _ => {}
            }
        }
        let status = child.wait().await.map_err(|error| RpcError::internal(format!("the CLI did not end cleanly: {error}")))?;
        end.report = last_message.as_deref().map(report_value);
        if end.report.is_none() && end.error.is_none() && !status.success() {
            end.error = Some(stderr.await.unwrap_or_default());
        }
        Ok(end)
    }

    // ---- shared --------------------------------------------------------------------------------

    fn spawn(&self, mut command: Command) -> Result<Child, RpcError> {
        command
            .current_dir(self.root)
            .envs(&self.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.spawn().map_err(|error| RpcError::new(codes::UNAVAILABLE, format!("cannot start {}: {error}", self.cli.display())))
    }

    async fn tool_call(&mut self, call_id: &str, name: &str, input: &Value) -> Result<(), RpcError> {
        self.step += 1;
        self.tool_calls += 1;
        let tool = name.strip_prefix(&format!("mcp__{MCP_SERVER}__")).unwrap_or(name);
        let raw_arguments = input.to_string();
        self.journal
            .append(
                kinds::TOOL_CALL,
                json!({"call_id": call_id, "tool": tool, "raw_name": name, "raw_arguments": raw_arguments, "args": input, "run_id": self.run_id, "step": self.step}),
            )
            .await?;
        Ok(())
    }

    async fn tool_result(&mut self, call_id: &str, ok: bool, output: &str) -> Result<(), RpcError> {
        self.journal.append(kinds::TOOL_RESULT, json!({"call_id": call_id, "ok": ok, "output": output, "run_id": self.run_id})).await?;
        Ok(())
    }
}

/// The task as the worker reads it: what to do, the planner's steps, the code it claims, and the
/// checks that say it is done; the step to take is last.
pub fn task_text(data: &Dataset, task: &Task) -> String {
    let mut text = data.text("frontier_task", &[("title", &task.title), ("brief", task.brief.trim())]);
    if !task.plan.trim().is_empty() {
        text.push_str(&data.text("frontier_plan", &[("plan", task.plan.trim())]));
    }
    if !task.touches.is_empty() {
        text.push_str(&data.text("frontier_touches", &[("touches", &task.touches.join(", "))]));
    }
    let checks: String = task.contract.checks.iter().filter(|check| check.kind == "command").map(|check| data.text("review_check", &[("command", &check.spec)])).collect();
    if !checks.is_empty() {
        text.push_str(&data.text("frontier_checks", &[("checks", &checks)]));
    }
    text.push_str(&data.text("frontier_go", &[]));
    text
}

/// How the run ended, from its last turn: the outcome, the summary and a question to the lead.
fn judge(data: &Dataset, turn: &TurnEnd) -> (String, String, Option<String>) {
    if let Some(error) = &turn.error {
        let error = error.trim();
        return ("failed".to_owned(), data.text("frontier_failed", &[("error", if error.is_empty() { "no reason given" } else { error })]), None);
    }
    let Some(report) = &turn.report else { return ("failed".to_owned(), data.text("frontier_no_report", &[]), None) };
    let summary = str_field(report, "summary").trim().to_owned();
    let question = str_field(report, "question").trim().to_owned();
    match str_field(report, "status") {
        "done" => ("done".to_owned(), summary, None),
        "waiting" if !question.is_empty() => ("waiting".to_owned(), summary, Some(question)),
        // Waiting without a question: nothing for the lead to answer, so the run is blocked.
        "waiting" | "blocked" => ("blocked".to_owned(), if summary.is_empty() { question } else { summary }, None),
        // Not the report's shape: its text is the summary, and the steward judges the work.
        _ => ("done".to_owned(), if summary.is_empty() { report.as_str().unwrap_or_default().to_owned() } else { summary }, None),
    }
}

/// A Claude turn's end: its structured report, or its error.
fn claude_turn_end(event: &Value) -> TurnEnd {
    let is_error = event.get("is_error").and_then(Value::as_bool).unwrap_or(false) || str_field(event, "subtype").starts_with("error");
    if is_error {
        let text = str_field(event, "result");
        let error = if text.is_empty() { str_field(event, "subtype").replace('_', " ") } else { text.to_owned() };
        return TurnEnd { report: None, error: Some(error) };
    }
    let report = match event.get("structured_output") {
        Some(output) if output.is_object() => Some(output.clone()),
        _ => Some(report_value(str_field(event, "result"))),
    };
    TurnEnd { report, error: None }
}

/// The report's JSON from a final message (its text when it is not JSON).
fn report_value(text: &str) -> Value {
    let trimmed = text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    serde_json::from_str::<Value>(trimmed).ok().filter(Value::is_object).unwrap_or_else(|| Value::String(text.to_owned()))
}

/// A TOML basic string (a JSON string's escapes are TOML's).
fn toml_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The text of an MCP or Claude tool result's content (a string or a list of parts).
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

async fn send_user(stdin: &mut ChildStdin, text: &str) -> Result<(), RpcError> {
    let message = json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}});
    let mut line = message.to_string();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.map_err(|error| RpcError::internal(format!("cannot send a message to the CLI: {error}")))?;
    stdin.flush().await.map_err(|error| RpcError::internal(format!("cannot send a message to the CLI: {error}")))
}

/// The end of a CLI's error output, read while it runs (so a full pipe never stops it).
fn tail_of(stderr: Option<tokio::process::ChildStderr>) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let Some(mut stderr) = stderr else { return String::new() };
        let mut kept: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    kept.extend_from_slice(&buffer[..read]);
                    if kept.len() > STDERR_TAIL * 2 {
                        kept.drain(..kept.len() - STDERR_TAIL);
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&kept).into_owned();
        let start = text.len().saturating_sub(STDERR_TAIL);
        let start = (start..text.len()).find(|index| text.is_char_boundary(*index)).unwrap_or(text.len());
        text[start..].trim().to_owned()
    })
}

/// The files a session changed in the project since a time, from the tools plugin's step log.
pub async fn changed_since(host: &Host, root: &str, session_id: &str, since_ms: u64) -> Vec<String> {
    let Ok(timeline) = host.get::<TimelineResult>(tools_methods::TIMELINE, json!({"workspace": root, "session_id": session_id, "since_ms": since_ms})).await else {
        return Vec::new();
    };
    let mut changed: Vec<String> = Vec::new();
    // Oldest first, so the list reads in the order the work went.
    for record in timeline.records.iter().rev().filter(|record| !record.undone) {
        for file in &record.files {
            if !changed.iter().any(|known| known == &file.path) {
                changed.push(file.path.clone());
            }
        }
    }
    changed
}
