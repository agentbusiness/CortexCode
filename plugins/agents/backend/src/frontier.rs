//! Frontier workers (docs/FRONTIER-AGENTS.md): an agent's task done by a frontier CLI, Claude Code
//! or Codex, run headless, instead of the chat flow's local agent loop.
//!
//! - **Who works.** The setting `agents.worker` chooses `local`, `claude` or `openai`; a frontier
//!   worker works when its CLI is found and signed in on this computer or on a member of the mesh
//!   that lends it, else the local agents do (and the choice says why: [`Frontier::workers`]).
//! - **Where the CLI runs.** On the computer that has it ([`Frontier::provider`]): this one first,
//!   else a member's, nearest first, one with a free place before one without. Either way the run
//!   is asked of that computer's `agents.frontier.run` ([`crate::lend`]); a session stays on the
//!   computer it began on, and a run that cannot go on there starts a new session from the files.
//! - **The CLI's hands are CortexCode's.** It has no tools of its own and sees no project folder.
//!   Its MCP messages come here as items of the run and are answered by `cx mcp` ([`Cli`]), the
//!   tools plugin's model tools held to the agent's session on this computer: the file
//!   reservations, the write sandbox, the step log and undo cover it as they cover a local agent.
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
use cc_protocol::contracts::frontier::{Offer, Offers, RunParams, RunResult, methods as lent};
use cc_protocol::contracts::memory::{AgentRecord, Task};
use cc_protocol::contracts::mesh::{PeersResult, methods as mesh_methods};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::records::methods as records_methods;
use cc_protocol::contracts::tools::{AgentEnvResult, TimelineResult, methods as tools_methods};
use cc_protocol::{CallStream, RpcError, codes};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::lend::{Lender, MCP_SERVER};

/// The setting that chooses who does agent work.
pub const WORKER_SETTING: &str = "agents.worker";
/// How long a detection (paths and sign-in, the members' offers) is reused before asking again.
const DETECT_FOR: Duration = Duration::from_secs(60);
/// Longest a sign-in check may take.
const STATUS_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest a member may take to say which CLIs it lends.
const OFFER_TIMEOUT: Duration = Duration::from_secs(4);
/// The event a waiting run's CLI stream carries here ([`Cli::next`]): the serving computer has no
/// free place yet.
const QUEUED_EVENT: &str = "cortex.queued";

/// The report every frontier run ends with (strict: every field required, nothing else).
fn report_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["status", "summary", "question"],
        "properties": {
            "status": {"type": "string", "enum": ["done", "waiting", "blocked", "declined"]},
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
    /// A plan run's plan (its report, in the plan's schema), when it wrote one.
    pub report: Option<Value>,
    /// The run could not go on with the agent's earlier CLI session (its computer cannot take it
    /// now): that session is no longer the agent's, whether or not a new one was started.
    pub moved: bool,
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

/// A CLI a member of the mesh lends.
#[derive(Clone)]
struct MemberOffer {
    node: String,
    computer: String,
    /// On the local network.
    near: bool,
    rtt_ms: u32,
    offer: Offer,
}

#[derive(Default)]
struct Offered {
    at: Option<Instant>,
    offers: Vec<MemberOffer>,
}

/// The computer a run's CLI runs on.
#[derive(Clone)]
pub struct Provider {
    /// The member (hex); None: this computer.
    pub node: Option<String>,
    /// The member's name (empty for this computer).
    pub computer: String,
    free: bool,
    near: u8,
    rtt_ms: u32,
}

/// The method and params that ask `method` of a run's computer: its frame directly here, through
/// the mesh on a member.
fn asked_at(node: &Option<String>, method: &str, params: Value) -> (String, Value) {
    match node {
        Some(node) => (mesh_methods::CALL.to_owned(), json!({"node": node, "method": method, "params": params})),
        None => (method.to_owned(), params),
    }
}

/// Frontier workers: detection, the CLIs lent here and by members, and the runs going now (for
/// steering).
#[derive(Default)]
pub struct Frontier {
    /// This computer's CLI runs, its own agents' and its members' ([`crate::lend`]).
    pub lender: Lender,
    offered: Mutex<Offered>,
    detected: Mutex<Detected>,
    /// Per worker: why its last run failed before doing any work (e.g. a CLI too old for its
    /// model). Kept until the CLIs are looked at again fresh (the settings panel opening, a
    /// choice) or one of its runs works.
    failures: Mutex<HashMap<String, String>>,
    live: Mutex<HashMap<AgentKey, mpsc::UnboundedSender<()>>>,
    /// Cancelled while the app's work is paused; a fresh one after Resume.
    pause: Mutex<CancellationToken>,
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
            lock(&self.offered).at = None;
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

    /// This computer's own CLIs: each with its version and why it cannot work now (None: it can).
    pub async fn local(&self, fresh: bool) -> Vec<(String, Option<String>, Option<String>)> {
        let found = self.detect(fresh).await;
        [workers::CLAUDE, workers::OPENAI]
            .into_iter()
            .map(|worker| match found.get(worker) {
                Some(found) => (worker.to_owned(), found.version.clone(), found.problem.clone()),
                None => (worker.to_owned(), None, Some("it was not looked for".to_owned())),
            })
            .collect()
    }

    /// Where this computer's CLI of a worker is, as the last detection found it.
    pub fn cli_path(&self, worker: &str) -> Option<PathBuf> {
        lock(&self.detected).found.get(worker).and_then(|found| found.path.clone())
    }

    /// The CLIs the members of the mesh lend now: every reachable member that runs an agents
    /// plugin is asked (`agents.frontier.offers`), and the answers are reused for [`DETECT_FOR`].
    /// Without a mesh, or alone in one, there are none.
    async fn member_offers(&self, host: &Host) -> Vec<MemberOffer> {
        {
            let offered = lock(&self.offered);
            if offered.at.is_some_and(|at| at.elapsed() < DETECT_FOR) {
                return offered.offers.clone();
            }
        }
        let peers = host.get::<PeersResult>(mesh_methods::PEERS, json!({})).await.map(|result| result.peers).unwrap_or_default();
        let mut offers = Vec::new();
        for peer in peers {
            // Another app on this machine has this computer's own CLIs: nothing more to find there.
            let lends = peer.reachable && !peer.member.local && peer.card.as_ref().is_some_and(|card| card.plugins.iter().any(|plugin| plugin.category == "agents" && plugin.active));
            if !lends {
                continue;
            }
            let (method, params) = asked_at(&Some(peer.member.node.clone()), lent::OFFERS, json!({}));
            let Ok(Ok(answer)) = tokio::time::timeout(OFFER_TIMEOUT, host.get::<Offers>(&method, params)).await else { continue };
            for offer in answer.workers {
                offers.push(MemberOffer { node: peer.member.node.clone(), computer: peer.member.name.clone(), near: peer.path == "lan", rtt_ms: peer.rtt_ms, offer });
            }
        }
        let mut offered = lock(&self.offered);
        offered.at = Some(Instant::now());
        offered.offers = offers.clone();
        offers
    }

    /// The computer a run of `worker` goes to: `home` (the computer its CLI session lives on) when
    /// that can still run it; else the best that can: one with a free place first, then this
    /// computer, then a member on the local network, then the nearest. None: nobody can run it now.
    pub async fn provider(&self, host: &Host, worker: &str, limit: u32, home: Option<&Option<String>>) -> Option<Provider> {
        let mut can = Vec::new();
        if self.detect(false).await.get(worker).is_some_and(|found| found.path.is_some() && found.problem.is_none()) {
            can.push(Provider { node: None, computer: String::new(), free: self.lender.running() < limit.max(1), near: 0, rtt_ms: 0 });
        }
        for member in self.member_offers(host).await {
            if member.offer.id == worker && member.offer.problem.is_none() {
                can.push(Provider {
                    node: Some(member.node),
                    computer: member.computer,
                    free: member.offer.running < member.offer.limit.max(1),
                    near: if member.near { 1 } else { 2 },
                    rtt_ms: member.rtt_ms,
                });
            }
        }
        if let Some(home) = home
            && let Some(provider) = can.iter().find(|provider| &provider.node == home)
        {
            return Some(provider.clone());
        }
        can.sort_by_key(|provider| (!provider.free, provider.near, provider.rtt_ms));
        can.into_iter().next()
    }

    /// Who does agent work: the choice, who works now, and every worker this computer has or a
    /// member lends it.
    pub async fn workers(&self, host: &Host, fresh: bool) -> WorkersResult {
        let found = self.detect(fresh).await;
        let lent_here = self.member_offers(host).await;
        let chosen = setting(host, WORKER_SETTING).await.filter(|worker| workers::ALL.contains(&worker.as_str())).unwrap_or_else(|| workers::LOCAL.to_owned());
        let mut list = vec![WorkerInfo { id: workers::LOCAL.to_owned(), label: "Local".to_owned(), installed: true, signed_in: true, ..WorkerInfo::default() }];
        for worker in [workers::CLAUDE, workers::OPENAI] {
            let (label, model, effort) = defaults(worker);
            let Found { path, problem, version } = found.get(worker).cloned().unwrap_or(Found { problem: Some("it was not looked for".to_owned()), ..Found::default() });
            let model = setting(host, &format!("agents.{worker}.model")).await.unwrap_or_else(|| model.to_owned());
            let effort = setting(host, &format!("agents.{worker}.effort")).await.unwrap_or_else(|| effort.to_owned());
            // Not able to work here: a member that lends it does the runs (the nearest first).
            let mut members: Vec<&MemberOffer> = lent_here.iter().filter(|member| member.offer.id == worker && member.offer.problem.is_none()).collect();
            members.sort_by_key(|member| (!member.near, member.rtt_ms));
            let member = if path.is_none() || problem.is_some() { members.first().copied() } else { None };
            let (installed, signed_in, version, problem) = match member {
                Some(member) => (true, true, member.offer.version.clone(), None),
                None => (path.is_some(), path.is_some() && problem.is_none(), version, problem),
            };
            // A run that could not start says what stops it (in the CLI's own words).
            let failure = lock(&self.failures).get(worker).cloned();
            let problem = problem.or_else(|| failure.map(|failure| format!("its last run could not start: {failure}")));
            list.push(WorkerInfo {
                id: worker.to_owned(),
                label: label.to_owned(),
                installed,
                signed_in,
                path: if member.is_some() { None } else { path.map(|path| path.to_string_lossy().into_owned()) },
                version,
                model: Some(model),
                effort: Some(effort),
                problem,
                node: member.map(|member| member.node.clone()),
                computer: member.map(|member| member.computer.clone()),
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

    /// Pause (true) or resume (false) the frontier agents' runs: a paused run stops at its next
    /// step boundary (no tool call in flight) and ends `paused`.
    pub fn set_paused(&self, paused: bool) {
        let mut pause = lock(&self.pause);
        if paused {
            pause.cancel();
        } else if pause.is_cancelled() {
            *pause = CancellationToken::new();
        }
    }

    fn pause_token(&self) -> CancellationToken {
        lock(&self.pause).clone()
    }

    /// Tell a running frontier agent that a steer waits in its journal (false: none is running).
    pub fn nudge(&self, key: &AgentKey) -> bool {
        lock(&self.live).get(key).is_some_and(|sender| sender.send(()).is_ok())
    }

    /// One run of a frontier agent on its task, until it reports, fails, or is cancelled.
    pub async fn run(&self, host: &Host, data: &Dataset, record: &AgentRecord, task: &Task, root: &str, cancel: &CancellationToken) -> Result<RunEnd, RpcError> {
        self.run_job(host, data, record, Job::Task(task), root, cancel).await
    }

    /// One run of a frontier agent that writes a plan and changes nothing: `text` is what to plan
    /// (the request and what the lead knows), `schema` the plan's JSON Schema. Its report is the
    /// plan ([`RunEnd::report`]).
    pub async fn plan(&self, host: &Host, data: &Dataset, record: &AgentRecord, text: &str, schema: &Value, root: &str, cancel: &CancellationToken) -> Result<RunEnd, RpcError> {
        self.run_job(host, data, record, Job::Plan { text, schema: strict_schema(schema) }, root, cancel).await
    }

    async fn run_job(&self, host: &Host, data: &Dataset, record: &AgentRecord, job: Job<'_>, root: &str, cancel: &CancellationToken) -> Result<RunEnd, RpcError> {
        let worker = record.worker.as_str();
        let listed = self.workers(host, false).await;
        let Some(info) = listed.workers.iter().find(|info| info.id == worker).cloned() else {
            return Err(RpcError::invalid_params(format!("`{worker}` is not a frontier worker")));
        };
        if let Some(problem) = &info.problem {
            return Err(RpcError::new(codes::UNAVAILABLE, format!("{} cannot work now: {problem}", info.label)));
        }
        let model = info.model.clone().unwrap_or_default();
        let effort = info.effort.clone().unwrap_or_default();
        let session_id = record.session_id.clone().ok_or_else(|| RpcError::internal(format!("agent {} has no session", record.name)))?;
        let access: AgentEnvResult = host.get(tools_methods::AGENT_ENV, json!({"session_id": session_id, "workspace": root})).await?;
        let mut env = access.env.clone();
        env.insert("CORTEX_AGENT".to_owned(), record.name.clone());
        // A planner's `cx mcp` offers the lead's tools: it looks and runs checks, it never edits.
        if matches!(job, Job::Plan { .. }) {
            env.insert("CORTEX_MCP_LEAD".to_owned(), "1".to_owned());
        }

        let journal = Journal::load(host.clone(), &session_id).await?;
        // The computer the CLI runs on. Its own session lives where its last run was (the journal's
        // last `running` status says): the run goes there again, and when that computer cannot
        // take it, a new session starts on another from what is in the files.
        let home: Option<Option<String>> = record.external_session.as_ref().and_then(|_| {
            journal
                .entries()
                .filter(|entry| entry.kind == kinds::STATUS && str_field(&entry.data, "state") == "running" && str_field(&entry.data, "worker") == worker)
                .last()
                .map(|entry| Some(str_field(&entry.data, "node").to_owned()).filter(|node| !node.is_empty()))
        });
        let limit = data.scheduler.frontier_parallel;
        let Some(provider) = self.provider(host, worker, limit, home.as_ref()).await else {
            return Err(RpcError::new(codes::UNAVAILABLE, format!("no computer can run {} now", info.label)));
        };
        let stays = home.as_ref() == Some(&provider.node);
        let external = record.external_session.clone().filter(|_| stays);
        let moved = record.external_session.is_some() && !stays;
        // The role prompt. Codex is not shown an MCP server's own instructions, so the tools guide
        // goes into its prompt; Claude Code reads it from the server.
        let mut prompt = data.prompt(job.prompt()).to_owned();
        if worker == workers::OPENAI
            && let Ok(listed) = host.call(tools_methods::MCP_LIST, json!({"workspace": root, "lead": matches!(job, Job::Plan { .. })})).await
            && let Some(guide) = listed.get("guide").and_then(Value::as_str).map(str::trim).filter(|guide| !guide.is_empty())
        {
            prompt.push_str("\n\n");
            prompt.push_str(guide);
        }
        let job_kind = if matches!(job, Job::Task(_)) { JobKind::Task } else { JobKind::Plan };
        let key = AgentKey { project_id: record.project_id.clone(), name: record.name.clone() };
        let (sender, nudges) = mpsc::unbounded_channel();
        lock(&self.live).insert(key.clone(), sender);
        let run = Run {
            host: host.clone(),
            data,
            job,
            root,
            session_id: session_id.clone(),
            run_id: format!("run-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            worker: worker.to_owned(),
            node: provider.node.clone(),
            computer: provider.computer.clone(),
            moved,
            prompt,
            opened: 0,
            model,
            effort,
            env,
            cx: access.cx.clone(),
            external,
            nudges,
            journal,
            step: 0,
            turns: 0,
            tool_calls: 0,
            usage: json!({}),
            agent: record.name.clone(),
            lead: host.session_get(&session_id).await.ok().and_then(|session| session.parent),
            refusal: None,
            // A plan is one step of its lead's turn: it is never paused on its own.
            pause: if matches!(job_kind, JobKind::Task) { self.pause_token() } else { CancellationToken::new() },
            in_flight: 0,
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

/// Which kind of job a run is (kept past the job itself).
#[derive(Clone, Copy)]
enum JobKind {
    Task,
    Plan,
}

/// What a frontier run is for.
enum Job<'a> {
    /// An agent's task: it changes the project, and reports done, waiting or blocked.
    Task(&'a Task),
    /// A plan: it reads and checks, changes nothing, and its report is the plan in `schema`.
    Plan { text: &'a str, schema: Value },
}

impl Job<'_> {
    fn task_id(&self) -> Option<&str> {
        match self {
            Job::Task(task) => Some(task.id.as_str()),
            Job::Plan { .. } => None,
        }
    }

    /// The role prompt (data: `prompts/<id>.md`).
    fn prompt(&self) -> &'static str {
        match self {
            Job::Task(_) => "worker",
            Job::Plan { .. } => "frontier_planner",
        }
    }

    fn schema(&self) -> Value {
        match self {
            Job::Task(_) => report_schema(),
            Job::Plan { schema, .. } => schema.clone(),
        }
    }
}

/// A JSON Schema both CLIs take in their strict modes: every object closed with all its
/// properties required, no length or count keywords, no host extensions, and an empty `enum`
/// (nothing to pick from in this conversation) as an empty list.
pub fn strict_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("array")
                && map.get("items").and_then(|items| items.get("enum")).and_then(Value::as_array).is_some_and(Vec::is_empty)
            {
                return json!({"type": "array", "items": {"type": "string"}, "maxItems": 0});
            }
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                if matches!(key.as_str(), "maxLength" | "minLength" | "maxItems" | "minItems" | "pattern" | "format") || key.starts_with("x-") {
                    continue;
                }
                // `properties` names fields: each value is a schema, each key a field kept as it is.
                let value = match (key.as_str(), value) {
                    ("properties", Value::Object(fields)) => Value::Object(fields.iter().map(|(name, field)| (name.clone(), strict_schema(field))).collect()),
                    _ => strict_schema(value),
                };
                out.insert(key.clone(), value);
            }
            if out.get("type").and_then(Value::as_str) == Some("object")
                && let Some(properties) = out.get("properties").and_then(Value::as_object)
            {
                let required: Vec<Value> = properties.keys().map(|key| Value::String(key.clone())).collect();
                out.insert("required".to_owned(), Value::Array(required));
                out.insert("additionalProperties".to_owned(), Value::Bool(false));
            }
            if out.get("enum").and_then(Value::as_array).is_some_and(|values| values.iter().all(Value::is_string)) && !out.contains_key("type") {
                out.insert("type".to_owned(), json!("string"));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(strict_schema).collect()),
        other => other.clone(),
    }
}

/// One frontier run in progress.
struct Run<'a> {
    host: Host,
    data: &'a Dataset,
    job: Job<'a>,
    root: &'a str,
    session_id: String,
    run_id: String,
    worker: String,
    /// The computer the CLI runs on: a member (hex), or None for this one; and its name.
    node: Option<String>,
    computer: String,
    /// The agent's CLI session was on a computer that cannot take this run: a new session starts
    /// here, told the task again and that the files hold the earlier work.
    moved: bool,
    /// The role prompt.
    prompt: String,
    /// CLI processes this run started (each needs a name of its own).
    opened: u32,
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
    /// The agent's name.
    agent: String,
    /// The lead session (the agent session's parent): it sees each message the agent writes.
    lead: Option<String>,
    /// The model stopped with a refusal (Claude's `stop_reason: "refusal"`), with what it said.
    refusal: Option<String>,
    /// Cancelled when the app's work is paused: the run stops at the next step boundary.
    pause: CancellationToken,
    /// Tool calls started and not answered yet (a pause waits for them).
    in_flight: u32,
}

/// What one CLI turn ended with.
#[derive(Default)]
struct TurnEnd {
    /// The report's JSON (or text that should have been it).
    report: Option<Value>,
    /// The CLI's own failure, in its words.
    error: Option<String>,
    /// The model refused (its API's refusal stop), with what it said.
    refused: Option<String>,
    /// The work was paused (the app's Pause): the CLI stopped at a step boundary.
    paused: bool,
}

impl Run<'_> {
    async fn go(mut self, cancel: &CancellationToken) -> Result<RunEnd, RpcError> {
        let started_ms = now_ms();
        let first = self.first_message().await?;
        self.journal
            .append(
                kinds::STATUS,
                json!({"state": "running", "run_id": self.run_id, "kind": "agent", "task_id": self.job.task_id(), "worker": self.worker, "model": self.model, "effort": self.effort, "node": self.node.clone().unwrap_or_default(), "computer": self.computer}),
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
        let mut report = None;
        let paused = matches!(&ended, Ok(turn) if turn.paused);
        if paused {
            // Its next run carries this as its first message: where it stopped, and to go on.
            let _ = self.journal.append(kinds::STEER, json!({"text": self.data.text("frontier_resume", &[]), "kind": "followup", "target": "chat", "consumed": false})).await;
        }
        let (outcome, summary, question) = match ended {
            Ok(_) if paused => ("paused".to_owned(), self.data.text("frontier_paused", &[]), None),
            Err(error) if error.code == codes::CANCELLED => ("cancelled".to_owned(), self.data.text("frontier_cancelled", &[]), None),
            Err(error) => ("failed".to_owned(), self.data.text("frontier_failed", &[("error", &error.message)]), None),
            Ok(turn) => match &self.job {
                Job::Task(_) => judge(self.data, &turn, self.tool_calls),
                // A plan is done when its report is the plan (an object with its tasks).
                Job::Plan { .. } => match (&turn.error, turn.report) {
                    (Some(error), _) => ("failed".to_owned(), self.data.text("frontier_failed", &[("error", error.trim())]), None),
                    (None, Some(plan)) if plan.get("tasks").is_some_and(Value::is_array) => {
                        let goal = str_field(&plan, "goal").trim().to_owned();
                        report = Some(plan);
                        ("done".to_owned(), goal, None)
                    }
                    (None, _) => ("failed".to_owned(), self.data.text("frontier_no_plan", &[]), None),
                },
            },
        };
        let result = AgentRunResult { outcome: outcome.clone(), summary: summary.clone(), turns: self.turns, tool_calls: self.tool_calls, changed_files, usage: self.usage.clone() };
        let _ = self
            .journal
            .append(kinds::STATUS, json!({"state": outcome, "detail": summary, "run_id": self.run_id, "task_id": self.job.task_id(), "result": result, "worker": self.worker}))
            .await;
        let status = match outcome.as_str() {
            "done" => SessionStatus::Done,
            // Declined: its task went to a local agent, so this session's work is over.
            "cancelled" | "declined" => SessionStatus::Cancelled,
            "waiting" | "blocked" | "paused" => SessionStatus::Waiting,
            _ => SessionStatus::Failed,
        };
        let _ = self.host.session_status(&self.session_id, status).await;
        // Failed with no tool call made: the CLI never got to the work (it did not start, or it
        // refused its model or its sign-in), so nothing of the task was done by it.
        let start_failed = result.outcome == "failed" && self.tool_calls == 0;
        Ok(RunEnd { result, external_session: self.external.clone(), question, start_failed, error: raw_error, report, moved: self.moved })
    }

    /// The run's first message: the task on its first run; else what waits for it (the lead's
    /// answers and steers), or the follow-up that continues the task.
    async fn first_message(&mut self) -> Result<String, RpcError> {
        let first_time = !self.journal.entries().any(|entry| entry.kind == kinds::USER_MESSAGE);
        let mut parts = Vec::new();
        // A new CLI session knows nothing yet: the project's own rules, then its job. One that
        // takes over from a session on another computer is told the files hold the earlier work.
        if first_time || self.moved {
            if let Some(contract) = project_contract(self.root) {
                parts.push(self.data.text("frontier_contract", &[("contract", contract.trim())]));
            }
            if self.moved && !first_time {
                parts.push(self.data.text("frontier_moved", &[]));
            }
            parts.push(match &self.job {
                Job::Task(task) => task_text(self.data, task),
                Job::Plan { text, .. } => (*text).to_owned(),
            });
        }
        parts.extend(self.take_steers().await?);
        if parts.is_empty() {
            parts.push(self.data.text("restart_followup", &[]));
        }
        let text = parts.join("\n\n");
        self.journal.append(kinds::USER_MESSAGE, json!({"text": text, "run_id": self.run_id, "task_id": self.job.task_id()})).await?;
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
        self.journal.append(kinds::USER_MESSAGE, json!({"text": text, "run_id": self.run_id, "task_id": self.job.task_id()})).await?;
        Ok(Some(text))
    }

    // ---- the CLI process --------------------------------------------------------------------------

    /// Start one CLI process on the run's computer with its first message.
    fn open(&mut self, message: String) -> Result<Cli, RpcError> {
        self.opened += 1;
        let params = RunParams {
            run: format!("{}-{}", self.run_id, self.opened),
            worker: self.worker.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            prompt: self.prompt.clone(),
            schema: self.job.schema(),
            session: self.session_id.clone(),
            resume: self.external.clone(),
            message,
        };
        Cli::start(&self.host, &self.node, params, &self.cx, &self.env, self.root)
    }

    /// The run waits for a free place on its computer: said once in the agent's journal, so the
    /// person watching sees why nothing moves yet.
    async fn queued(&mut self, event: &Value) -> Result<(), RpcError> {
        let ahead = event.get("ahead").and_then(Value::as_u64).unwrap_or(0).to_string();
        let computer = if self.computer.is_empty() { "this computer".to_owned() } else { self.computer.clone() };
        let text = self.data.text("frontier_queued", &[("worker", label(&self.worker)), ("computer", &computer), ("ahead", &ahead)]);
        self.journal.append(kinds::NOTE, json!({"text": text, "purpose": "frontier_queued", "host_message": false, "run_id": self.run_id})).await?;
        Ok(())
    }

    // ---- Claude Code ---------------------------------------------------------------------------

    async fn claude(&mut self, first: String, cancel: &CancellationToken) -> Result<TurnEnd, RpcError> {
        let mut cli = self.open(first)?;
        let mut last = TurnEnd::default();
        let mut pausing = false;
        let mut input_open = true;
        loop {
            // Paused, with no tool call in flight: the CLI stops here (dropping it ends it); its
            // session keeps the conversation, and the resumed run goes on from it.
            if pausing && self.in_flight == 0 {
                return Ok(TurnEnd { paused: true, ..TurnEnd::default() });
            }
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(RpcError::cancelled()),
                _ = self.pause.cancelled(), if !pausing => {
                    pausing = true;
                    continue;
                }
                event = cli.next() => event,
            };
            let Some(event) = event else { break };
            match str_field(&event, "type") {
                QUEUED_EVENT => self.queued(&event).await?,
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
                    if let Some(refusal) = self.refusal.take() {
                        last.refused = Some(refusal);
                    }
                    // The lead's answers and steers that came during the turn are its next message;
                    // with none, the input ends and the CLI finishes.
                    match self.next_message().await? {
                        Some(text) if last.error.is_none() && last.refused.is_none() && !pausing && input_open => cli.say(&text).await?,
                        _ => {
                            if input_open {
                                input_open = false;
                                cli.close().await;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        settle(&mut last, cli.end().await)?;
        Ok(last)
    }

    async fn claude_assistant(&mut self, event: &Value) -> Result<(), RpcError> {
        // A refusal stop: the model will not go on with this work (what it said is the reason).
        if event.pointer("/message/stop_reason").and_then(Value::as_str) == Some("refusal") {
            let said: Vec<&str> = event.pointer("/message/content").and_then(Value::as_array).into_iter().flatten().filter_map(|part| part.get("text").and_then(Value::as_str)).collect();
            let said = said.join(" ").trim().to_owned();
            self.refusal = Some(if said.is_empty() { "the model stopped with a refusal".to_owned() } else { said });
        }
        for part in event.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
            match str_field(part, "type") {
                "text" => {
                    let text = str_field(part, "text");
                    if !text.trim().is_empty() {
                        self.assistant_message(&text).await?;
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
        let mut message = first;
        loop {
            let turn = self.codex_turn(message, cancel).await;
            match turn {
                // Paused between turns: the next turn waits for Resume.
                Ok(turn) if turn.error.is_none() && !turn.paused && self.pause.is_cancelled() => break Ok(TurnEnd { paused: true, ..TurnEnd::default() }),
                Ok(turn) if turn.error.is_none() && !turn.paused => match self.next_message().await {
                    Ok(Some(text)) => message = text,
                    Ok(None) => break Ok(turn),
                    Err(error) => break Err(error),
                },
                other => break other,
            }
        }
    }

    /// One Codex turn: one CLI process (a later turn resumes its thread).
    async fn codex_turn(&mut self, message: String, cancel: &CancellationToken) -> Result<TurnEnd, RpcError> {
        let mut cli = self.open(message)?;
        let mut end = TurnEnd::default();
        let mut last_message: Option<String> = None;
        let mut pausing = false;
        loop {
            // Paused, with no tool call in flight: the CLI stops here (dropping it ends it); its
            // session keeps the conversation, and the resumed run goes on from it.
            if pausing && self.in_flight == 0 {
                return Ok(TurnEnd { paused: true, ..TurnEnd::default() });
            }
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(RpcError::cancelled()),
                _ = self.pause.cancelled(), if !pausing => {
                    pausing = true;
                    continue;
                }
                event = cli.next() => event,
            };
            let Some(event) = event else { break };
            match str_field(&event, "type") {
                QUEUED_EVENT => self.queued(&event).await?,
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
                                self.assistant_message(&text).await?;
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
        end.report = last_message.as_deref().map(report_value);
        settle(&mut end, cli.end().await)?;
        Ok(end)
    }

    // ---- shared --------------------------------------------------------------------------------

    /// A message the agent wrote: in its own journal, and as a progress line on the lead's card
    /// (not a message its model reads: the agent's `tell_lead` and its report are those), so the
    /// person following the lead sees each agent's work as it goes.
    async fn assistant_message(&mut self, text: &str) -> Result<(), RpcError> {
        self.journal.append(kinds::ASSISTANT_MESSAGE, json!({"text": text, "run_id": self.run_id})).await?;
        if let Some(lead) = &self.lead {
            let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(400).collect();
            let _ = self
                .host
                .journal_append(lead, kinds::NOTE, json!({"text": format!("{}: {line}", self.agent), "purpose": "agent_progress", "host_message": false, "agent": self.agent, "task_id": self.job.task_id()}))
                .await;
        }
        Ok(())
    }

    async fn tool_call(&mut self, call_id: &str, name: &str, input: &Value) -> Result<(), RpcError> {
        self.step += 1;
        self.tool_calls += 1;
        self.in_flight += 1;
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
        self.in_flight = self.in_flight.saturating_sub(1);
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
fn judge(data: &Dataset, turn: &TurnEnd, tool_calls: u32) -> (String, String, Option<String>) {
    // The model's own refusal (its API's refusal stop): the task is the local model's.
    if let Some(refusal) = &turn.refused {
        return ("declined".to_owned(), refusal.trim().to_owned(), None);
    }
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
        // It will not do the task: the local model takes it (with the model's reason).
        "declined" => ("declined".to_owned(), summary, None),
        // Not the report's shape, with no tool call made: it answered in prose instead of working
        // (a refusal in its own words), so the task is the local model's.
        _ if tool_calls == 0 => ("declined".to_owned(), if summary.is_empty() { report.as_str().unwrap_or_default().trim().to_owned() } else { summary }, None),
        // Not the report's shape after working: its text is the summary, and the steward judges
        // the work.
        _ => ("done".to_owned(), if summary.is_empty() { report.as_str().unwrap_or_default().to_owned() } else { summary }, None),
    }
}

/// A Claude turn's end: its structured report, or its error.
fn claude_turn_end(event: &Value) -> TurnEnd {
    let is_error = event.get("is_error").and_then(Value::as_bool).unwrap_or(false) || str_field(event, "subtype").starts_with("error");
    if is_error {
        let text = str_field(event, "result");
        let error = if text.is_empty() { str_field(event, "subtype").replace('_', " ") } else { text.to_owned() };
        return TurnEnd { report: None, error: Some(error), refused: None, paused: false };
    }
    let report = match event.get("structured_output") {
        Some(output) if output.is_object() => Some(output.clone()),
        _ => Some(report_value(str_field(event, "result"))),
    };
    TurnEnd { report, error: None, refused: None, paused: false }
}

/// The report's JSON from a final message (its text when it is not JSON).
fn report_value(text: &str) -> Value {
    let trimmed = text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    serde_json::from_str::<Value>(trimmed).ok().filter(Value::is_object).unwrap_or_else(|| Value::String(text.to_owned()))
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

/// How a CLI process ended, into the turn's end: a CLI that failed without an answer says why in
/// its own words; a run that could not be asked of its computer at all is the run's failure.
fn settle(turn: &mut TurnEnd, ended: Result<RunResult, RpcError>) -> Result<(), RpcError> {
    let answered = turn.report.is_some() || turn.error.is_some();
    match ended {
        Ok(result) => {
            if !answered && !result.ok {
                turn.error = Some(result.error);
            }
            Ok(())
        }
        Err(_) if answered => Ok(()),
        Err(error) => Err(error),
    }
}

/// The project's own rules as its repository states them (`AGENTS.md`, else `CLAUDE.md`): the CLI
/// runs outside the project's folder, so it reads them in its first message.
fn project_contract(root: &str) -> Option<String> {
    ["AGENTS.md", "CLAUDE.md"].iter().find_map(|name| std::fs::read_to_string(Path::new(root).join(name)).ok()).filter(|text| !text.trim().is_empty())
}

/// One CLI process on the computer that runs it (this one or a member), with this computer's tools
/// answering it: `cx mcp` runs here for the agent's session, the CLI's MCP messages come as items
/// of the run and go into it, and its answers go back to the run's computer.
struct Cli {
    host: Host,
    node: Option<String>,
    run: String,
    /// The run's call; taken when the run is ended properly ([`Cli::end`]).
    stream: Option<CallStream>,
    to_tools: mpsc::UnboundedSender<String>,
    _tools: Child,
}

impl Drop for Cli {
    /// A CLI left before its end (a stop, a pause, an error here) is ended on its computer.
    fn drop(&mut self) {
        if let Some(stream) = &self.stream {
            stream.cancel();
        }
    }
}

impl Cli {
    fn start(host: &Host, node: &Option<String>, params: RunParams, cx: &str, env: &BTreeMap<String, String>, root: &str) -> Result<Cli, RpcError> {
        let mut tools = Command::new(cx)
            .arg("mcp")
            .current_dir(root)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| RpcError::new(codes::UNAVAILABLE, format!("cannot start the agent's tools ({cx} mcp): {error}")))?;
        let mut tools_in = tools.stdin.take().ok_or_else(|| RpcError::internal("the agent's tools have no stdin"))?;
        let tools_out = tools.stdout.take().ok_or_else(|| RpcError::internal("the agent's tools have no stdout"))?;
        // What the CLI asks of its tools, one message a line, in the order it came.
        let (to_tools, mut for_tools) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(mut line) = for_tools.recv().await {
                line.push('\n');
                if tools_in.write_all(line.as_bytes()).await.is_err() || tools_in.flush().await.is_err() {
                    break;
                }
            }
        });
        // What the tools answer goes back to the CLI on its computer.
        let run = params.run.clone();
        {
            let host = host.clone();
            let node = node.clone();
            let run = run.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(tools_out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                    let (method, params) = asked_at(&node, lent::MCP, json!({"run": run, "message": message}));
                    if host.call(&method, params).await.is_err() {
                        break;
                    }
                }
            });
        }
        let params = serde_json::to_value(&params).map_err(|error| RpcError::internal(error.to_string()))?;
        let (method, params) = asked_at(node, lent::RUN, params);
        let stream = host.ctx().call_stream(&method, params)?;
        Ok(Cli { host: host.clone(), node: node.clone(), run, stream: Some(stream), to_tools, _tools: tools })
    }

    /// The CLI's next output event; None once the process ended. Its MCP messages are handed to
    /// the tools on the way, and a wait for a place comes as a [`QUEUED_EVENT`].
    async fn next(&mut self) -> Option<Value> {
        loop {
            let item = self.stream.as_mut()?.next().await?;
            if let Some(message) = item.get("mcp").filter(|message| !message.is_null()) {
                let _ = self.to_tools.send(message.to_string());
            } else if let Some(ahead) = item.get("queued").and_then(Value::as_u64) {
                return Some(json!({"type": QUEUED_EVENT, "ahead": ahead}));
            } else if let Some(event) = item.get("event").filter(|event| !event.is_null()) {
                return Some(event.clone());
            }
        }
    }

    /// The CLI's next message (Claude Code takes it when its turn ended).
    async fn say(&self, text: &str) -> Result<(), RpcError> {
        let (method, params) = asked_at(&self.node, lent::SAY, json!({"run": self.run, "text": text}));
        self.host.call(&method, params).await.map(|_| ())
    }

    /// No further message comes: the CLI ends.
    async fn close(&self) {
        let (method, params) = asked_at(&self.node, lent::CLOSE, json!({"run": self.run}));
        let _ = self.host.call(&method, params).await;
    }

    /// How the process ended, once its output is read to the end.
    async fn end(mut self) -> Result<RunResult, RpcError> {
        let Some(stream) = self.stream.take() else { return Err(RpcError::internal("the run was ended already")) };
        let value = stream.finish().await?;
        serde_json::from_value(value).map_err(|error| RpcError::internal(format!("the run's end could not be read: {error}")))
    }
}

/// The files a session changed in the project since a time, from the tools plugin's step log: the
/// net change of each file (as it was before the session's first step that touched it against
/// after its last), so a file that came and went (build output, a scratch file the run's cleanup
/// removed) or ended as it began is not a change, and an undone step counts for nothing.
pub async fn changed_since(host: &Host, root: &str, session_id: &str, since_ms: u64) -> Vec<String> {
    let Ok(timeline) = host.get::<TimelineResult>(tools_methods::TIMELINE, json!({"workspace": root, "session_id": session_id, "since_ms": since_ms})).await else {
        return Vec::new();
    };
    // Per file, in the order the work went: its content before the first step, after the last.
    let mut net: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for record in timeline.records.iter().rev().filter(|record| !record.undone && record.undoes.is_empty()) {
        for file in &record.files {
            match at.get(&file.path) {
                Some(&index) => net[index].2 = file.after.clone(),
                None => {
                    at.insert(file.path.clone(), net.len());
                    net.push((file.path.clone(), file.before.clone(), file.after.clone()));
                }
            }
        }
    }
    net.into_iter().filter(|(_, before, after)| before != after).map(|(path, _, _)| path).collect()
}
