//! What a job's session did, counted from its journal: the run's state, the plan and the task in
//! work, the tool calls since that task started, tokens, the approval and the replan that wait.
//! Counting only; nothing here judges the work.
//!
//! A [`Track`] is fed the session's entries in order ([`Track::entry`]: a new entry, also every
//! entry of a journal read again after a restart) and the changes to them ([`Track::set`],
//! [`Track::append`]). The chat flow writes what is read here (`plugins/chatflow`): `status`
//! entries (the run and its stage), `plan` entries (the tasks, and `steps_done` as each task ends),
//! `note` entries (`task_start`, `replan_held`, `context`), `tool_call` / `tool_result`, `usage`,
//! `approval`, `error`, `steer`, `agent_spawned`.

use cc_protocol::contracts::chatflow::kinds as chatflow_kinds;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::runner::{HeldApproval, HeldReplan, Task};
use serde_json::Value;
use std::collections::HashMap;

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64, continued from `hash`: equality of bytes, not meaning. Hashing a text in pieces
/// gives the hash of the whole.
fn fnv(mut hash: u64, text: &str) -> u64 {
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// JSON text with every object's keys sorted, so equal values print equally.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys.into_iter().map(|key| format!("{key:?}:{}", canonical(&map[key]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => format!("[{}]", items.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// The command a `run` call is about: its first command that is not `cd`, `echo`, `sleep` or
/// `printf`, without what follows a pipe and without redirections (as the chat flow tells a check
/// run from other commands, `repetition::base_command`).
pub fn base_command(command: &str) -> String {
    command
        .split([';', '\n'])
        .flat_map(|part| part.split("&&"))
        .flat_map(|part| part.split("||"))
        .map(|part| part.split('|').next().unwrap_or("").split_whitespace().take_while(|word| !word.contains('>') && !word.starts_with('<')).collect::<Vec<_>>().join(" "))
        .find(|part| !part.is_empty() && !["cd", "echo", "sleep", "printf"].contains(&part.split(' ').next().unwrap_or("")))
        .unwrap_or_default()
}

/// One task of the session's plan, as the chat flow holds it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanTask {
    pub n: u32,
    pub title: String,
    pub brief: String,
    pub known: Vec<String>,
    pub rules: Vec<String>,
    pub depends_on: Vec<u32>,
    pub touches: Vec<String>,
    pub checks: Vec<String>,
}

impl PlanTask {
    fn read(value: &Value) -> Self {
        Self {
            n: value.get("n").and_then(Value::as_u64).unwrap_or(0) as u32,
            title: str_field(value, "title").to_owned(),
            brief: str_field(value, "brief").to_owned(),
            known: strings(value.get("known")),
            rules: strings(value.get("rules")),
            depends_on: value.get("depends_on").and_then(Value::as_array).map(|items| items.iter().filter_map(|item| item.as_u64().map(|n| n as u32)).collect()).unwrap_or_default(),
            touches: strings(value.get("touches")),
            checks: strings(value.get("checks")),
        }
    }

    /// As the manager gets it (`request` is the runner's to fill in).
    pub fn task(&self) -> Task {
        Task {
            n: self.n,
            title: self.title.clone(),
            brief: self.brief.clone(),
            known: self.known.clone(),
            rules: self.rules.clone(),
            depends_on: self.depends_on.clone(),
            touches: self.touches.clone(),
            checks: self.checks.clone(),
            request: String::new(),
        }
    }
}

/// One tool call of the task in work.
#[derive(Debug, Clone, Default)]
struct Call {
    call_id: String,
    tool: String,
    /// What it is about, for the "now" line (a path, a command, a query).
    target: String,
    /// The command of a `run` call, reduced ([`base_command`]).
    command: String,
    /// Tool and arguments.
    asked: u64,
    ts_ms: u64,
    /// The host made it (a task's own discovery), not the model.
    host: bool,
    /// None until its result is in.
    ok: Option<bool>,
    /// A command that ran and failed.
    failed: bool,
    /// The call did not run (skipped for a message, interrupted).
    skipped: bool,
    /// Its result's text.
    answered: u64,
    result_seq: u64,
}

/// What changed with an entry or a patch: what the runner acts on at once.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// The session's plan task `n` started.
    TaskStarted(u32),
    /// Plan task `n` ended with `outcome` (`done`: its check passed).
    TaskEnded { n: u32, outcome: String },
    /// The run ended (a final status).
    RunEnded,
    /// A replan waits for an answer, or stopped waiting.
    Replan,
    /// An approval question waits (its entry's seq), or was answered (0).
    Approval(u64),
    /// The session started an agent in a session of its own.
    Spawned(String),
    /// The plan was written again (an accepted replan).
    PlanChanged,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Status,
    Plan,
    Approval,
    Held,
    Result,
}

/// The counts of one session's journal.
#[derive(Debug, Clone, Default)]
pub struct Track {
    /// The newest run: its id, its state (running | done | failed | cancelled | blocked |
    /// stopped; empty before the first run), its final detail and its stage.
    pub run_id: String,
    pub state: String,
    pub detail: String,
    pub stage: String,
    status_seq: u64,
    /// The run reported that the conversation no longer fits the model's context.
    pub context_full: bool,
    /// The last `error` entry of the run.
    pub error_code: String,
    pub error_message: String,
    /// The newest plan version: its entry, its tasks and how each ended so far.
    plan_seq: u64,
    pub plan_tasks: Vec<PlanTask>,
    pub steps_done: Vec<(u32, String)>,
    /// The plan task in work (1-based; 0: none started).
    pub task: u32,
    calls: Vec<Call>,
    /// When the manager last wrote to the builder (calls are counted from it).
    told_ms: u64,
    pub tokens: u64,
    pub first_ms: u64,
    pub last_ms: u64,
    pub approval: Option<HeldApproval>,
    pub replan: Option<HeldReplan>,
    replan_seq: u64,
    /// Sessions of agents this session started.
    pub spawned: Vec<String>,
    seen: HashMap<u64, Kind>,
    /// The highest seq taken in: an entry at or below it is a repeat.
    pub through: u64,
}

impl Track {
    pub fn running(&self) -> bool {
        self.state == "running"
    }

    /// Take in one entry (in seq order). An entry seen before is ignored.
    pub fn entry(&mut self, entry: &JournalEntry) -> Vec<Change> {
        if entry.seq <= self.through {
            return Vec::new();
        }
        self.through = entry.seq;
        if self.first_ms == 0 {
            self.first_ms = entry.ts_ms;
        }
        self.last_ms = self.last_ms.max(entry.ts_ms);
        let data = &entry.data;
        let mut changes = Vec::new();
        match entry.kind.as_str() {
            kinds::STATUS => {
                let state = str_field(data, "state");
                if state == "running" {
                    self.run_id = str_field(data, "run_id").to_owned();
                    self.state = "running".into();
                    self.detail.clear();
                    self.stage = str_field(data, "stage").to_owned();
                    self.status_seq = entry.seq;
                    self.context_full = false;
                    self.error_code.clear();
                    self.error_message.clear();
                    self.seen.insert(entry.seq, Kind::Status);
                } else if !state.is_empty() && str_field(data, "run_id") == self.run_id {
                    self.state = state.to_owned();
                    self.detail = str_field(data, "detail").to_owned();
                    self.stage.clear();
                    changes.push(Change::RunEnded);
                }
            }
            chatflow_kinds::PLAN => {
                let changed = self.plan_seq != 0;
                self.plan_seq = entry.seq;
                self.seen.insert(entry.seq, Kind::Plan);
                self.plan_fields(data);
                if changed {
                    changes.push(Change::PlanChanged);
                }
            }
            kinds::NOTE => match str_field(data, "purpose") {
                "task_start" => {
                    let n = data.get("n").and_then(Value::as_u64).unwrap_or(0) as u32;
                    self.task = n;
                    self.calls.clear();
                    self.told_ms = 0;
                    changes.push(Change::TaskStarted(n));
                }
                "context" => self.context_full = true,
                "replan_held" => {
                    self.seen.insert(entry.seq, Kind::Held);
                    if str_field(data, "state") == "pending" {
                        let tasks = data.get("tasks").and_then(Value::as_array).map(|tasks| tasks.iter().map(|task| PlanTask::read(task).task()).collect()).unwrap_or_default();
                        self.replan = Some(HeldReplan { found: str_field(data, "found").to_owned(), tasks });
                        self.replan_seq = entry.seq;
                        changes.push(Change::Replan);
                    }
                }
                _ => {}
            },
            kinds::TOOL_CALL => {
                let args = data.get("args").cloned().unwrap_or(Value::Null);
                let tool = str_field(data, "tool").to_owned();
                let text = |name: &str| args.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
                let command = text("command");
                let target = ["path", "target", "command", "query", "pattern", "url", "name"].iter().map(|name| text(name)).find(|value| !value.trim().is_empty()).unwrap_or_default();
                self.calls.push(Call {
                    call_id: str_field(data, "call_id").to_owned(),
                    asked: fnv(fnv(FNV_START, &tool), &canonical(&args)),
                    command: base_command(&command),
                    target,
                    tool,
                    ts_ms: entry.ts_ms,
                    host: data.get("host").and_then(Value::as_bool) == Some(true),
                    ..Call::default()
                });
            }
            kinds::TOOL_RESULT => {
                let call_id = str_field(data, "call_id");
                if let Some(call) = self.calls.iter_mut().rev().find(|call| call.call_id == call_id) {
                    let ok = data.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let exit = data.pointer("/display/exit_code").and_then(Value::as_i64);
                    call.ok = Some(ok);
                    call.skipped = ["skipped", "interrupted_call", "needs_approval"].iter().any(|flag| data.get(*flag).and_then(Value::as_bool) == Some(true));
                    call.failed = call.tool == "run" && !call.skipped && (exit.is_some_and(|code| code != 0) || (!ok && exit.is_none()));
                    call.answered = fnv(FNV_START, str_field(data, "output"));
                    call.result_seq = entry.seq;
                    self.seen.insert(entry.seq, Kind::Result);
                }
            }
            kinds::USAGE => {
                let count = |name: &str| data.get(name).and_then(Value::as_u64).unwrap_or(0);
                self.tokens += count("prompt_tokens") + count("completion_tokens");
            }
            kinds::ERROR => {
                self.error_code = str_field(data, "code").to_owned();
                self.error_message = str_field(data, "message").to_owned();
            }
            kinds::STEER => self.told_ms = entry.ts_ms,
            kinds::AGENT_SPAWNED => {
                let session = str_field(data, "session_id").to_owned();
                if !session.is_empty() && !self.spawned.contains(&session) {
                    self.spawned.push(session.clone());
                    changes.push(Change::Spawned(session));
                }
            }
            chatflow_kinds::APPROVAL => {
                self.seen.insert(entry.seq, Kind::Approval);
                if str_field(data, "state") == "pending" {
                    self.approval = Some(HeldApproval { seq: entry.seq, tool: str_field(data, "tool").to_owned(), classes: strings(data.get("classes")), question: str_field(data, "question").to_owned() });
                    changes.push(Change::Approval(entry.seq));
                }
            }
            _ => {}
        }
        changes
    }

    fn plan_fields(&mut self, data: &Value) {
        self.plan_tasks = data.get("tasks").and_then(Value::as_array).map(|tasks| tasks.iter().map(PlanTask::read).collect()).unwrap_or_default();
        self.steps_done = Self::steps(data.get("steps_done"));
    }

    fn steps(value: Option<&Value>) -> Vec<(u32, String)> {
        value
            .and_then(Value::as_array)
            .map(|steps| steps.iter().map(|step| (step.get("n").and_then(Value::as_u64).unwrap_or(0) as u32, str_field(step, "outcome").to_owned())).collect())
            .unwrap_or_default()
    }

    /// A field of an entry was set.
    pub fn set(&mut self, seq: u64, field: &str, value: &Value) -> Vec<Change> {
        let mut changes = Vec::new();
        match self.seen.get(&seq).copied() {
            Some(Kind::Status) if seq == self.status_seq && field == "stage" && self.running() => {
                self.stage = value.as_str().unwrap_or("").to_owned();
            }
            Some(Kind::Plan) if seq == self.plan_seq => match field {
                "steps_done" => {
                    let steps = Self::steps(Some(value));
                    for (n, outcome) in steps.iter().skip(self.steps_done.len()) {
                        changes.push(Change::TaskEnded { n: *n, outcome: outcome.clone() });
                    }
                    self.steps_done = steps;
                }
                "tasks" => self.plan_tasks = value.as_array().map(|tasks| tasks.iter().map(PlanTask::read).collect()).unwrap_or_default(),
                _ => {}
            },
            Some(Kind::Approval) if field == "state" && value.as_str() != Some("pending") => {
                if self.approval.as_ref().is_some_and(|approval| approval.seq == seq) {
                    self.approval = None;
                    changes.push(Change::Approval(0));
                }
            }
            Some(Kind::Held) if field == "state" && value.as_str() != Some("pending") && seq == self.replan_seq => {
                if self.replan.take().is_some() {
                    changes.push(Change::Replan);
                }
            }
            _ => {}
        }
        changes
    }

    /// Text was appended to a field of an entry (a result's output while it streams).
    pub fn append(&mut self, seq: u64, field: &str, text: &str) {
        if field == "output"
            && self.seen.get(&seq) == Some(&Kind::Result)
            && let Some(call) = self.calls.iter_mut().rev().find(|call| call.result_seq == seq)
        {
            call.answered = fnv(call.answered, text);
        }
    }

    /// The model's own calls of the task in work that have their result.
    fn finished(&self) -> impl Iterator<Item = &Call> {
        self.calls.iter().filter(|call| !call.host && call.ok.is_some())
    }

    /// Successful calls that were not edits since the last edit (as the chat flow's MANAGE counts
    /// them, `manage::progress`): a run of one of the task's `checks` is a check run, not reading,
    /// and calls from before the manager's last message are not counted.
    pub fn calls_since_edit(&self, checks: &[String]) -> u32 {
        let check_bases: Vec<String> = checks.iter().map(|command| base_command(command)).filter(|base| !base.is_empty()).collect();
        let mut since = 0;
        for call in self.finished() {
            if call.tool == "edit" && call.ok == Some(true) {
                since = 0;
            } else if call.ts_ms >= self.told_ms && call.ok == Some(true) && !(call.tool == "run" && !call.command.is_empty() && check_bases.contains(&call.command)) {
                since += 1;
            }
        }
        since
    }

    /// The longest run of the same call with the same result, one after the other.
    pub fn same_call(&self) -> u32 {
        let mut longest = 0;
        let mut run = 0;
        let mut before: Option<(u64, u64)> = None;
        for call in self.finished().filter(|call| !call.skipped) {
            let this = (call.asked, call.answered);
            run = if before == Some(this) { run + 1 } else { 1 };
            before = Some(this);
            longest = longest.max(run);
        }
        longest
    }

    /// The longest run of the same failing command (other calls in between do not end it; the
    /// command passing, or another command failing, does), and that command.
    pub fn same_failure(&self) -> (u32, String) {
        let mut longest: (u32, String) = (0, String::new());
        let mut run: (u32, String) = (0, String::new());
        for call in self.finished().filter(|call| call.tool == "run" && !call.skipped && !call.command.is_empty()) {
            if call.failed {
                run = if run.1 == call.command { (run.0 + 1, run.1) } else { (1, call.command.clone()) };
                if run.0 > longest.0 {
                    longest = run.clone();
                }
            } else if run.1 == call.command {
                run = (0, String::new());
            }
        }
        longest
    }

    /// The model's last tool call: its tool and what it is about.
    pub fn last_call(&self) -> Option<(&str, &str)> {
        self.calls.iter().rev().find(|call| !call.host).map(|call| (call.tool.as_str(), call.target.as_str()))
    }

    /// Whether every task of the plan ended done.
    pub fn plan_done(&self) -> bool {
        !self.plan_tasks.is_empty() && self.plan_tasks.iter().all(|task| self.steps_done.iter().any(|(n, outcome)| *n == task.n && outcome == "done"))
    }
}
