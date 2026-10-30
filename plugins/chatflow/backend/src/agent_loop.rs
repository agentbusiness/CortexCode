//! The coding agent loop: the model works with native tools (read, edit, run) in the session's
//! workspace; the host executes every call through `tools.execute`, journals everything, injects
//! impact after signature changes, delivers steering at tool boundaries, stops repetition
//! structurally and, for tasks with a contract, keeps going until the host-run contract is green
//! or the model reports it is blocked.
//!
//! Every piece of loop state is recoverable from the journal (steps, consumed steers, repetition
//! history, verification failures), so a successor instance resumes a parked run where it stopped.

use crate::context::Prefix;
use crate::data::Dataset;
use crate::contract;
use crate::host::{self, Host};
use crate::impact::Tracker;
use crate::journal::Journal;
use crate::llm::{self, CallSpec, StreamTo};
use crate::prompts;
use crate::reads::Scope;
use crate::repetition::{Detector, Verdict};
use crate::schemas::max_effort;
use cc_protocol::contracts::agents::{VerifyReport, methods as agents_methods};
use cc_protocol::contracts::chatflow::{kinds as chatflow_kinds, topics as chatflow_topics};
use cc_protocol::contracts::model::methods as model_methods;
use cc_protocol::contracts::frame::SessionStatus;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::memory::{BoardListResult, Task, methods as memory_methods};
use cc_protocol::contracts::model::{ChatMessage, Priority, ToolCall};
use cc_protocol::contracts::tools::{ExecContext, ToolOutcome, methods as tools_methods};
use cc_protocol::{RpcError, codes};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A read shorter than this is shown again even when it repeats one in view: its pointer would
/// save little.
const UNCHANGED_MIN_CHARS: usize = 400;

/// A read target split into its file and line range: `src/a.rs:40-80` is `src/a.rs` with lines 40
/// to 80, `src/a.rs:40+10` lines 40 to 49, `src/a.rs:40` line 40 on (no end); a target without a
/// range (or a URI) has none.
fn read_lines(target: &str) -> (String, Option<(usize, Option<usize>)>) {
    let target = target.trim().trim_start_matches("./");
    let Some((file, range)) = target.rsplit_once(':').filter(|(file, range)| {
        !file.contains("://") && !range.is_empty() && range.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | '+'))
    }) else {
        return (target.to_owned(), None);
    };
    let number = |text: &str| text.trim().parse::<usize>().ok();
    let lines = if let Some((start, count)) = range.split_once('+') {
        number(start).zip(number(count)).filter(|(_, count)| *count > 0).map(|(start, count)| (start, Some(start + count - 1)))
    } else if let Some((start, end)) = range.split_once('-') {
        number(start).map(|start| (start, if end.trim().is_empty() { None } else { number(end) }))
    } else {
        number(range).map(|start| (start, None))
    };
    (file.to_owned(), lines)
}

/// What a read call reads: its `path`, with its `range` argument joined as `path:range` when the
/// call gave one apart.
fn read_target(args: &Value) -> String {
    let path = args.get("path").and_then(Value::as_str).unwrap_or("").trim();
    match args.get("range").and_then(Value::as_str).map(str::trim).filter(|range| !range.is_empty()) {
        Some(range) if !path.contains("://") => format!("{path}:{}", range.trim_start_matches(':')),
        _ => path.to_owned(),
    }
}

/// The newest earlier successful read (its result seq and output) that shows every line `target`
/// (a ranged read) asks for, with no change to the file since: a whole read of it (`mode: file`)
/// or a range that contains the asked one. None for a read without a range, or when the file
/// changed after that read.
fn covered_by_earlier_read(entries: &[JournalEntry], target: &str) -> Option<(u64, String)> {
    let (file, Some((start, end))) = read_lines(target) else { return None };
    let end = end?;
    let calls: HashMap<&str, &JournalEntry> =
        entries.iter().filter(|entry| entry.kind == kinds::TOOL_CALL).map(|entry| (str_field(&entry.data, "call_id"), entry)).collect();
    // Called before the read's own result is journaled: every result here is an earlier one.
    for result in entries.iter().rev().filter(|entry| entry.kind == kinds::TOOL_RESULT) {
        let changed = result.data.get("changed_files").and_then(Value::as_array).is_some_and(|files| {
            files.iter().filter_map(Value::as_str).any(|path| path.trim_start_matches("./") == file || path.ends_with(&format!("/{file}")))
        });
        if changed {
            return None;
        }
        let Some(call) = calls.get(str_field(&result.data, "call_id")) else { continue };
        if str_field(&call.data, "tool") != "read" || !result.data.get("ok").and_then(Value::as_bool).unwrap_or(false) || result.data.get("unchanged").is_some() {
            continue;
        }
        let earlier_target = call.data.get("args").map(read_target).unwrap_or_default();
        let (earlier_file, earlier_lines) = read_lines(&earlier_target);
        if earlier_file != file {
            continue;
        }
        let covers = match earlier_lines {
            None => result.data.get("display").and_then(|display| display.get("mode")).and_then(Value::as_str) == Some("file"),
            Some((from, Some(to))) => from <= start && end <= to,
            Some((_, None)) => false,
        };
        if covers {
            return Some((result.seq, str_field(&result.data, "output").to_owned()));
        }
    }
    None
}

/// How long a parking instance lets an in-flight tool call finish before cancelling it. The frame
/// gives `plugin.shutdown` 5 s, so parking (this grace plus journaling) must fit inside that.
pub const PARK_GRACE: Duration = Duration::from_secs(3);

/// Signals that stop a run.
#[derive(Clone, Default)]
pub struct Stops {
    /// The user (or the agents plugin) cancelled the run.
    pub cancel: CancellationToken,
    /// This instance is shutting down: stop at a boundary without a terminal status, so the
    /// successor resumes.
    pub park: CancellationToken,
}

/// Cancels the forwarding task when its caller finishes, including error exits.
pub struct CombinedStop(CancellationToken);

impl std::ops::Deref for CombinedStop {
    type Target = CancellationToken;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for CombinedStop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Stops {
    /// Fires on either signal (for calls that must stop promptly either way).
    pub fn any(&self) -> CombinedStop {
        let token = CancellationToken::new();
        let (cancel, park, out) = (self.cancel.clone(), self.park.clone(), token.clone());
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => out.cancel(),
                _ = park.cancelled() => out.cancel(),
                _ = out.cancelled() => {}
            }
        });
        CombinedStop(token)
    }
}

#[derive(Clone)]
pub struct RunEnv {
    pub host: Host,
    pub session_id: String,
    pub scope: Scope,
    pub prefix: Prefix,
    pub run_id: String,
    pub mode: String,
    pub priority: Priority,
    pub stops: Stops,
    /// The data set (prompts, texts, sampling, limits) this run uses.
    pub data: std::sync::Arc<Dataset>,
    /// Current stage, shown by `chatflow.state`.
    pub stage: std::sync::Arc<std::sync::Mutex<String>>,
    /// The model takes image content parts (`model.info`); attached images are then shown to it.
    pub image_input: bool,
}

impl RunEnv {
    pub fn set_stage(&self, stage: &str) {
        if let Ok(mut current) = self.stage.lock() {
            *current = stage.to_owned();
        }
    }

    pub fn cache_key(&self) -> String {
        self.session_id.clone()
    }

    /// A model call of `stage` at `effort` over `messages`, with the stage's sampling.
    #[allow(clippy::too_many_arguments)]
    pub fn call(&self, stage: &str, effort: &str, messages: Vec<ChatMessage>, grammar: Option<String>, tool_choice: Option<Value>, step: u64, content: bool) -> CallSpec {
        let effort = self.data.effort_for(stage, effort);
        CallSpec {
            stage: stage.to_owned(),
            messages,
            tools: self.prefix.tools.clone(),
            json_schema: None,
            grammar,
            tool_choice,
            sampling: self.data.sampling(stage, effort != "none"),
            effort,
            priority: self.priority,
            cache_key: self.cache_key(),
            stream: StreamTo { run_id: self.run_id.clone(), step, content, live: Vec::new() },
            think_budget: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum LoopKind {
    /// The foreground DO route.
    Foreground,
    /// A question (the router's LOOKUP or ANSWER): explore with the tools and answer.
    Question,
    /// A named background agent working a task.
    Agent { name: String },
}

pub struct LoopSpec {
    pub kind: LoopKind,
    pub effort: String,
    /// A task with a contract: the host verifies before the run counts as done.
    pub task: Option<Task>,
    /// The one step of the run's plan this loop carries out (a DO decision's plan runs step by step).
    pub step: Option<StepSpec>,
}

/// One step of a plan for the foreground loop: its number and text, and the steps done before it.
#[derive(Clone, Debug, PartialEq)]
pub struct StepSpec {
    pub n: u32,
    pub of: u32,
    pub text: String,
    pub done: Vec<crate::plan::StepDone>,
    /// The whole work in one sentence (the plan's goal): the frame the task sits in.
    pub goal: String,
    /// The titles of the tasks after this one, done later (named so the model leaves them).
    pub later: Vec<String>,
}

/// The stage text that tells the loop its step: the step, what the earlier steps did, and that
/// the host takes the reply as the end of this step (the plan's checks run after the last).
pub fn step_live(data: &Dataset, step: &StepSpec) -> String {
    let done = if step.done.is_empty() {
        String::new()
    } else {
        let items: String = step.done.iter().map(|done| data.text("do_step_done_item", &[("n", &done.n.to_string()), ("summary", done.summary.trim())])).collect();
        data.text("do_step_done", &[("items", &items)])
    };
    let later = if step.later.is_empty() { String::new() } else { data.text("do_step_later", &[("titles", &step.later.join("; "))]) };
    let goal = if step.goal.trim().is_empty() { String::new() } else { data.text("do_step_goal", &[("goal", step.goal.trim())]) };
    let mut text = data.text("do_step", &[("goal", &goal), ("n", &step.n.to_string()), ("of", &step.of.to_string()), ("text", step.text.trim()), ("later", &later), ("done", &done)]);
    if step.n >= step.of {
        text.push_str(&data.text("do_step_last", &[]));
    }
    text
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoopOutcome {
    /// done | failed | cancelled | blocked | stopped | parked
    pub outcome: String,
    pub summary: String,
    pub turns: u32,
    pub tool_calls: u32,
    pub changed_files: Vec<String>,
    pub verified: Option<bool>,
}

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

fn of_run<'a>(entries: &'a [JournalEntry], run_id: &'a str) -> impl Iterator<Item = &'a JournalEntry> + 'a {
    entries.iter().filter(move |entry| str_field(&entry.data, "run_id") == run_id)
}

/// The next model step number of a run (steps are journaled on every model entry).
pub fn next_step(entries: &[JournalEntry], run_id: &str) -> u64 {
    of_run(entries, run_id).filter_map(|entry| entry.data.get("step").and_then(Value::as_u64)).max().unwrap_or(0) + 1
}

/// Close what a stopped instance left open: streaming entries are marked interrupted (the step is
/// generated again) and calls without results get an honest result (their effect is unknown, so
/// they are never replayed blindly).
pub async fn repair_interrupted(journal: &mut Journal, run_id: &str, data: &Dataset) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    for entry in of_run(&entries, run_id) {
        let streaming = entry.data.get("streaming").and_then(Value::as_bool).unwrap_or(false);
        if streaming && matches!(entry.kind.as_str(), kinds::ASSISTANT_MESSAGE | kinds::REASONING) {
            journal.set(entry.seq, "interrupted", json!(true)).await?;
            journal.set(entry.seq, "streaming", json!(false)).await?;
        }
    }
    let results: HashSet<String> = entries
        .iter()
        .filter(|entry| entry.kind == kinds::TOOL_RESULT)
        .map(|entry| str_field(&entry.data, "call_id").to_owned())
        .collect();
    // A call that asked the user for approval is not interrupted: the run resumes waiting for it.
    let asked: HashSet<String> = entries
        .iter()
        .filter(|entry| entry.kind == chatflow_kinds::APPROVAL)
        .map(|entry| str_field(&entry.data, "call_id").to_owned())
        .collect();
    for entry in of_run(&entries, run_id).filter(|entry| entry.kind == kinds::TOOL_CALL) {
        let call_id = str_field(&entry.data, "call_id");
        if !results.contains(call_id) && !asked.contains(call_id) {
            let host_read = entry.data.get("host").and_then(Value::as_bool).unwrap_or(false);
            journal
                .append(
                    kinds::TOOL_RESULT,
                    json!({
                        "call_id": call_id,
                        "ok": false,
                        "output": data.text("interrupted", &[]),
                        "duration_ms": 0,
                        "run_id": run_id,
                        "host": host_read,
                        "interrupted_call": true,
                    }),
                )
                .await?;
        }
    }
    Ok(())
}

/// Rebuild the repetition detector from the run's journal.
fn detector_from(entries: &[JournalEntry], run_id: &str, repeat_at: u32) -> Detector {
    let mut detector = Detector::new(repeat_at);
    let calls: std::collections::HashMap<&str, &JournalEntry> = of_run(entries, run_id)
        .filter(|entry| entry.kind == kinds::TOOL_CALL)
        .map(|entry| (str_field(&entry.data, "call_id"), entry))
        .collect();
    for entry in of_run(entries, run_id) {
        match entry.kind.as_str() {
            kinds::TOOL_RESULT if !entry.data.get("host").and_then(Value::as_bool).unwrap_or(false) => {
                if let Some(call) = calls.get(str_field(&entry.data, "call_id")) {
                    let args = call.data.get("args").cloned().unwrap_or(Value::Null);
                    let _ = detector.observe_call(str_field(&call.data, "tool"), &args, str_field(&entry.data, "output"));
                }
                if entry.data.get("changed_files").and_then(Value::as_array).is_some_and(|files| !files.is_empty()) {
                    detector.changed();
                }
            }
            kinds::USER_MESSAGE => detector.reset(),
            // Each plan task gets its own correction before a repeat stops it.
            kinds::NOTE if str_field(&entry.data, "purpose") == "task_start" => detector.reset(),
            _ => {}
        }
    }
    detector
}

fn changed_files_of(entries: &[JournalEntry], run_id: &str) -> BTreeSet<String> {
    of_run(entries, run_id)
        .filter(|entry| entry.kind == kinds::TOOL_RESULT)
        .flat_map(|entry| {
            entry.data.get("changed_files").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter_map(|value| value.as_str().map(str::to_owned))
        })
        .collect()
}

/// Deliver pending `steer` entries at this tool boundary. Returns their texts.
pub async fn deliver_steers(journal: &mut Journal) -> Result<Vec<String>, RpcError> {
    let pending = journal.pending_steers("steer");
    for steer in &pending {
        journal.consume_steer(steer.seq).await?;
    }
    Ok(pending.iter().map(|steer| str_field(&steer.data, "text").trim().to_owned()).filter(|text| !text.is_empty()).collect())
}

/// An agent's team: the other open tasks of its tree (same parent), by id, with its own task.
pub struct Team {
    pub me: Task,
    pub others: HashMap<String, Task>,
}

impl Team {
    /// The tree of `task` as memory holds it now (no tree: a team of one).
    pub async fn of(env: &RunEnv, task: &Task) -> Team {
        let mut others = HashMap::new();
        if let Some(parent) = &task.parent_id
            && let Ok(list) = env.host.get::<cc_protocol::contracts::memory::TaskListResult>(memory_methods::TASK_LIST, json!({"project_id": task.project_id, "parent_id": parent})).await
        {
            others = list.tasks.into_iter().filter(|other| other.id != task.id).map(|other| (other.id.clone(), other)).collect();
        }
        Team { me: task.clone(), others }
    }

    /// The team as the agent's stage line names it: each other agent with its task (none: empty).
    pub fn live(&self, data: &Dataset) -> String {
        let members: Vec<String> = self
            .others
            .values()
            .filter(|task| !matches!(task.status.as_str(), "done" | "cancelled"))
            .filter_map(|task| task.assignee.as_ref().map(|name| data.text("team_member", &[("name", name), ("title", &task.title)])))
            .collect();
        if members.is_empty() { String::new() } else { data.text("team_live", &[("members", &members.join(", "))]) }
    }

    /// Whether a task's work bears on this agent's: one waits for the other, or they touch the
    /// same files or symbols.
    fn related(&self, other: &Task) -> bool {
        self.me.depends_on.contains(&other.id)
            || other.depends_on.contains(&self.me.id)
            || other.touches.iter().any(|touch| self.me.touches.contains(touch))
    }
}

/// Board kinds that ask or decide something: from a teammate they always reach the agent.
const ASKING_KINDS: &[&str] = &["need", "question", "decision", "proposal"];

/// Deliver the board messages that bear on this agent since the last delivery (the host decides,
/// so the model sees few): those addressed to it; from its team, what asks or decides something
/// and what is about a task related to its own (one waits for the other, or they touch the same
/// code); from outside its team, only a claim on code it touches. A newer message from the same
/// author about the same task replaces the older one.
async fn deliver_board(journal: &mut Journal, env: &RunEnv, name: &str, team: Option<&Team>) -> Result<(), RpcError> {
    // The first delivery starts at the session's start: what the board said before the agent
    // existed is earlier plans' history (seen: thirty "was cancelled" notices from replaced plans
    // in front of every new agent's first step).
    let session_start = journal.entries().next().map(|entry| entry.ts_ms).unwrap_or(0);
    let since = journal
        .entries()
        .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "board")
        .filter_map(|entry| entry.data.get("board_ts").and_then(Value::as_u64))
        .max()
        .unwrap_or(session_start);
    let Ok(list) = env
        .host
        .get::<BoardListResult>(memory_methods::BOARD_LIST, json!({"project_id": env.scope.project_id, "since_ms": since + 1, "to": name}))
        .await
    else {
        return Ok(());
    };
    let mut kept: Vec<&cc_protocol::contracts::memory::BoardMessage> = Vec::new();
    for message in list.messages.iter().filter(|message| message.author != name && message.ts_ms > since) {
        let relevant = match (&message.to, team) {
            (Some(_), _) | (None, None) => true,
            (None, Some(team)) => {
                let about = message.refs.iter().find_map(|reference| team.others.get(reference));
                match about {
                    Some(task) => ASKING_KINDS.contains(&message.kind.as_str()) || team.related(task),
                    None if message.kind == "claim" => match message.refs.first() {
                        Some(reference) => env
                            .host
                            .get::<Task>(memory_methods::TASK_GET, json!({"id": reference}))
                            .await
                            .is_ok_and(|task| task.touches.iter().any(|touch| team.me.touches.contains(touch))),
                        None => false,
                    },
                    // The lead and the user speak to the whole team; a post about a task outside
                    // the agent's tree (another plan's) is not for it.
                    None => (message.refs.is_empty() || message.refs.contains(&team.me.id)) && matches!(message.author.as_str(), "lead" | "user" | "chat"),
                }
            }
        };
        if !relevant {
            continue;
        }
        let topic = message.refs.first();
        kept.retain(|earlier| !(earlier.author == message.author && earlier.refs.first() == topic && topic.is_some()));
        kept.push(message);
    }
    // Each message is recorded with its own time; the newest of the whole batch marks the delivery
    // so skipped ones are not looked at again.
    let newest = list.messages.iter().map(|message| message.ts_ms).max().unwrap_or(since);
    for message in &kept {
        let text = env.data.text("board_message", &[("author", &message.author), ("kind", &message.kind), ("text", &message.text)]);
        journal
            .append(
                kinds::NOTE,
                json!({"text": text, "purpose": "board", "run_id": env.run_id, "host_message": true, "board_ts": message.ts_ms.max(newest)}),
            )
            .await?;
    }
    if kept.is_empty() && newest > since {
        // Nothing relevant: the mark moves on without a message to the model.
        journal.append(kinds::NOTE, json!({"text": "", "purpose": "board", "run_id": env.run_id, "host_message": false, "board_ts": newest})).await?;
    }
    Ok(())
}

/// Post `text` (an edit's impact) to each open teammate whose task changes one of the `reached`
/// files: they build on the same code, so they learn of it from the host instead of finding a
/// broken call an hour later. Best effort: a failed post only leaves them to find it.
async fn tell_reached(env: &RunEnv, name: &str, team: &Team, reached: &HashSet<String>, text: &str) {
    for task in team.others.values().filter(|task| !matches!(task.status.as_str(), "done" | "cancelled")) {
        let Some(assignee) = task.assignee.as_deref() else { continue };
        if !task.touches.iter().any(|touch| reached.contains(touch.trim_start_matches("./"))) {
            continue;
        }
        let text = env.data.text("board_reached", &[("agent", name), ("text", text)]);
        let _ = env.host.call(memory_methods::BOARD_POST, json!({"project_id": env.scope.project_id, "kind": "decision", "author": name, "text": text, "refs": [task.id], "to": assignee})).await;
    }
}

/// Whether the research plugin is active (`frame.plugins.list`): the structural fact behind
/// offering a lookup as a next step. A failure to ask counts as not available.
async fn research_available(env: &RunEnv) -> bool {
    let Ok(listed) = env.host.get::<Value>("frame.plugins.list", json!({})).await else { return false };
    listed
        .get("plugins")
        .and_then(Value::as_array)
        .is_some_and(|plugins| plugins.iter().any(|plugin| plugin.get("category").and_then(Value::as_str) == Some("research") && plugin.get("state").and_then(Value::as_str) == Some("active")))
}

enum Exec {
    Done(Box<ToolOutcome>),
    Failed(String),
    Stopped,
}

/// Bytes of a running command's output journaled live on its `tool_call` entry (`live_output`),
/// so the person sees it run; the result carries the whole output afterwards.
const LIVE_OUTPUT_BYTES: usize = 8 * 1024;

/// A running call's output on its way to the journal: chunks are batched (no journal op per byte)
/// and capped at [`LIVE_OUTPUT_BYTES`].
struct LiveOutput {
    seq: Option<u64>,
    pending: String,
    sent: usize,
    flushed: std::time::Instant,
}

impl LiveOutput {
    fn new(seq: Option<u64>) -> Self {
        Self { seq, pending: String::new(), sent: 0, flushed: std::time::Instant::now() }
    }

    /// Take in one streamed item of the tools plugin (`{"kind": "output", "text": ...}`).
    fn push(&mut self, item: &Value) {
        if self.seq.is_none() || self.sent >= LIVE_OUTPUT_BYTES || item.get("kind").and_then(Value::as_str) != Some("output") {
            return;
        }
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            let room = LIVE_OUTPUT_BYTES - self.sent;
            let take: String = text.chars().take(room).collect();
            self.sent += take.len();
            self.pending.push_str(&take);
            if self.sent >= LIVE_OUTPUT_BYTES {
                self.pending.push_str("\n[… the rest is in the result]\n");
            }
        }
    }

    /// What to journal now: the pending text once it is big or old enough.
    fn due(&mut self) -> Option<(u64, String)> {
        let seq = self.seq?;
        if self.pending.is_empty() || (self.pending.len() < 256 && self.flushed.elapsed() < Duration::from_millis(300)) {
            return None;
        }
        self.flushed = std::time::Instant::now();
        Some((seq, std::mem::take(&mut self.pending)))
    }
}

async fn execute_call(env: &RunEnv, journal: &mut Journal, call_seq: Option<u64>, call: &ToolCall) -> Exec {
    let params = json!({
        "context": ExecContext {
            project_id: env.scope.project_id.clone(),
            session_id: env.session_id.clone(),
            workspace: env.scope.workspace.clone(),
            mode: env.mode.clone(),
            edit_format: None,
        },
        "call": call,
    });
    let mut delay = Duration::from_millis(200);
    let started = std::time::Instant::now();
    loop {
        let mut stream = match env.host.ctx().call_stream(tools_methods::EXECUTE, params.clone()) {
            Ok(stream) => stream,
            Err(error) => return Exec::Failed(env.data.text("call_unsent", &[("error", &error.to_string())])),
        };
        let park_grace = async {
            env.stops.park.cancelled().await;
            tokio::time::sleep(PARK_GRACE).await;
        };
        tokio::pin!(park_grace);
        // A background agent waiting on a long tool holds no model slot: hibernate its KV.
        let hibernate_after = env.data.limits.hibernate_after_ms;
        let hibernate = async {
            if env.priority != Priority::Agent || hibernate_after == 0 {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(Duration::from_millis(hibernate_after)).await;
        };
        tokio::pin!(hibernate);
        let mut hibernated = false;
        let mut live = LiveOutput::new(call_seq);
        let outcome = loop {
            tokio::select! {
                _ = &mut hibernate, if !hibernated => {
                    hibernated = true;
                    let host = env.host.clone();
                    let key = env.cache_key();
                    let session = env.session_id.clone();
                    tokio::spawn(async move {
                        match host.call(model_methods::HIBERNATE, json!({"cache_key": key})).await {
                            Ok(_) => host.ctx().publish(chatflow_topics::HIBERNATED, json!({"session_id": session, "cache_key": key})),
                            Err(error) => tracing::debug!(%error, "model.hibernate failed"),
                        }
                    });
                }
                item = stream.next() => {
                    let Some(item) = item else { break stream.finish().await };
                    live.push(&item);
                    if let Some((seq, text)) = live.due() {
                        let _ = journal.append_text(seq, "live_output", &text).await;
                    }
                }
                _ = env.stops.cancel.cancelled() => {
                    stream.cancel();
                    let _ = tokio::time::timeout(Duration::from_millis(300), async { while stream.next().await.is_some() {} }).await;
                    return Exec::Stopped;
                }
                _ = &mut park_grace => {
                    stream.cancel();
                    return Exec::Stopped;
                }
            }
        };
        match outcome {
            Ok(value) => {
                return match serde_json::from_value::<ToolOutcome>(value) {
                    Ok(outcome) => Exec::Done(Box::new(outcome)),
                    Err(error) => Exec::Failed(env.data.text("call_unreadable", &[("error", &error.to_string())])),
                };
            }
            Err(error) if host::refused(&error) && started.elapsed() < Duration::from_secs(180) => {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = env.stops.cancel.cancelled() => return Exec::Stopped,
                }
                delay = (delay * 2).min(Duration::from_secs(2));
            }
            Err(error) if host::interrupted(&error) => {
                return Exec::Failed(env.data.text("tools_stopped", &[("error", &error.to_string())]));
            }
            Err(error) => return Exec::Failed(env.data.text("call_failed", &[("error", &error.to_string())])),
        }
    }
}

enum Approval {
    /// Approved and executed.
    Approved(Box<ToolOutcome>),
    /// Denied, or the approved call failed: the text the model gets as the result.
    Answered(String),
    /// The run was cancelled or parked while waiting (the entry stays for the successor).
    Stopped,
}

/// Wait for the user's answer to an ask-mode call (an `approval` entry the UI answers through
/// `chatflow.approve`), then run the call again with the approved classes.
async fn await_approval(env: &RunEnv, journal: &mut Journal, call: &ToolCall, outcome: &ToolOutcome, existing: Option<u64>) -> Result<Approval, RpcError> {
    // A replay (`replay.rs`) has nobody to ask: what needs approval is not run.
    if crate::replay::arm(&journal.entries_vec()).is_some() {
        return Ok(Approval::Answered(env.data.text("replay_denied", &[])));
    }
    let classes: Vec<String> = outcome
        .display
        .pointer("/approval/classes")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let seq = match existing {
        Some(seq) => seq,
        None => {
            journal
                .append(
                    chatflow_kinds::APPROVAL,
                    json!({"call_id": call.id, "tool": outcome.tool, "args": outcome.args, "classes": classes, "question": outcome.output, "state": "pending", "run_id": env.run_id}),
                )
                .await?
        }
    };
    let Some(entry) = wait_for_answer(env, seq).await? else { return Ok(Approval::Stopped) };
    let classes: Vec<String> = entry
        .data
        .get("classes")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    if str_field(&entry.data, "state") != "approved" {
        return Ok(Approval::Answered(env.data.text("approval_denied", &[("classes", &classes.join(", "))])));
    }
    env.host.call(tools_methods::APPROVE, json!({"session_id": env.session_id, "classes": classes})).await?;
    Ok(match execute_call(env, journal, None, call).await {
        Exec::Done(outcome) => Approval::Approved(outcome),
        Exec::Failed(message) => Approval::Answered(message),
        Exec::Stopped => Approval::Stopped,
    })
}

/// Wait for the user's answer to approval entry `seq` (the session shows it needs them meanwhile);
/// the answered entry, or None when the run stopped while waiting.
pub async fn wait_for_answer(env: &RunEnv, seq: u64) -> Result<Option<JournalEntry>, RpcError> {
    let _ = env.host.session_status(&env.session_id, SessionStatus::Waiting).await;
    env.set_stage("approval");
    let entry = loop {
        let fresh = env.host.journal_read(&env.session_id, seq).await?;
        let Some(entry) = fresh.into_iter().find(|entry| entry.seq == seq) else {
            return Err(RpcError::internal("the approval entry vanished"));
        };
        if str_field(&entry.data, "state") != "pending" {
            break entry;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            _ = env.stops.cancel.cancelled() => return Ok(None),
            _ = env.stops.park.cancelled() => return Ok(None),
        }
    };
    let _ = env.host.session_status(&env.session_id, SessionStatus::Running).await;
    env.set_stage("act");
    Ok(Some(entry))
}

/// Finish approvals left open by a stopped predecessor. Returns false when the run stopped while
/// waiting.
async fn resolve_pending_approvals(env: &RunEnv, journal: &mut Journal) -> Result<bool, RpcError> {
    let entries = journal.entries_vec();
    let answered: HashSet<String> = entries.iter().filter(|entry| entry.kind == kinds::TOOL_RESULT).map(|entry| str_field(&entry.data, "call_id").to_owned()).collect();
    let open: Vec<JournalEntry> = of_run(&entries, &env.run_id)
        .filter(|entry| entry.kind == chatflow_kinds::APPROVAL && !answered.contains(str_field(&entry.data, "call_id")))
        .cloned()
        .collect();
    for approval in open {
        let call_id = str_field(&approval.data, "call_id").to_owned();
        let Some(call_entry) = entries.iter().find(|entry| entry.kind == kinds::TOOL_CALL && str_field(&entry.data, "call_id") == call_id) else { continue };
        let call = ToolCall {
            id: call_id.clone(),
            name: str_field(&call_entry.data, "raw_name").to_owned(),
            arguments: str_field(&call_entry.data, "raw_arguments").to_owned(),
        };
        let pending: ToolOutcome = serde_json::from_value(json!({
            "ok": false,
            "output": str_field(&approval.data, "question"),
            "display": {"approval": {"classes": approval.data.get("classes").cloned().unwrap_or(json!([]))}},
            "tool": str_field(&approval.data, "tool"),
            "args": approval.data.get("args").cloned().unwrap_or(Value::Null),
            "needs_approval": true,
            "duration_ms": 0,
        }))?;
        match await_approval(env, journal, &call, &pending, Some(approval.seq)).await? {
            Approval::Stopped => return Ok(false),
            Approval::Answered(text) => {
                journal.append(kinds::TOOL_RESULT, json!({"call_id": call_id, "ok": false, "output": text, "duration_ms": 0, "run_id": env.run_id})).await?;
            }
            Approval::Approved(outcome) => {
                let mut data = json!({
                    "call_id": call_id, "ok": outcome.ok, "output": outcome.output, "display": outcome.display,
                    "duration_ms": outcome.duration_ms, "changed_files": outcome.changed_files, "run_id": env.run_id,
                });
                if let Some(artifact) = &outcome.artifact {
                    data["artifact"] = json!(artifact);
                }
                journal.append(kinds::TOOL_RESULT, data).await?;
            }
        }
    }
    Ok(true)
}

/// Whether a lesson's text pipes a command's output into a filter (`| grep`, `| tail`, `| head`,
/// or says to pipe to one): structural, the shell syntax and the filter's name.
fn filters_output(text: &str) -> bool {
    let compact: String = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    ["grep", "tail", "head"].iter().any(|filter| compact.contains(&format!("| {filter}")) || compact.contains(&format!("|{filter}")) || compact.contains(&format!("pipe to `{filter}")) || compact.contains(&format!("pipe to {filter}")) || compact.contains(&format!("pipe it to `{filter}")) || compact.contains(&format!("pipe it to {filter}")))
}

/// Verify a task's contract: through `agents.verify` (which records the report and the task's
/// status), or with the shared contract runner when no agents plugin serves.
/// The note under a failure with `key`: the lessons and this project's facts learned from it
/// before, each shown once in the run (`shown`), their ids kept on the note so learning after the
/// run can tell whether the failure came again. Nothing when nothing was learned from it.
async fn failure_note(env: &RunEnv, journal: &mut Journal, key: &str, shown: &mut HashSet<String>) -> Result<(), RpcError> {
    let languages: Vec<String> = journal
        .context_entry()
        .and_then(|entry| entry.data.get("languages").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|language| language.as_str().map(str::to_owned))
        .collect();
    let mut found = crate::learn::at_failure(&env.host, &env.data, &env.scope.project_id, key, &languages, shown).await;
    // A lesson that cuts a command's output down teaches the filtered reruns the run result already
    // makes needless (seen: "pipe cargo check to grep" under every failure, then twenty filtered
    // runs of one check).
    found.retain(|(item, _)| !filters_output(&item.text));
    if found.is_empty() {
        return Ok(());
    }
    let lines: Vec<String> = found.iter().map(|(item, _)| env.data.text("list_item", &[("text", item.text.trim())])).collect();
    let lessons: Vec<&str> = found.iter().filter(|(_, project)| !project).map(|(item, _)| item.id.as_str()).collect();
    let facts: Vec<&str> = found.iter().filter(|(_, project)| *project).map(|(item, _)| item.id.as_str()).collect();
    shown.extend(found.iter().map(|(item, _)| item.id.clone()));
    let text = env.data.text("lessons_failure", &[("text", &lines.join("\n"))]);
    journal
        .append(kinds::NOTE, json!({"text": text, "purpose": "lesson", "run_id": env.run_id, "host_message": true, "key": key, "lessons": lessons, "facts": facts}))
        .await
        .map(|_| ())
}

pub async fn verify_task(host: &Host, journal: &mut Journal, task: &Task, workspace: &str, cancel: &CancellationToken) -> Result<VerifyReport, RpcError> {
    let via_agents = host
        .call_with(agents_methods::VERIFY, json!({"task_id": task.id, "workspace": workspace}), host::Effect::Idempotent, Some(cancel))
        .await;
    match via_agents {
        Ok(value) => cc_protocol::parse_params(value),
        Err(error) if error.code == codes::UNAVAILABLE || error.code == codes::UNKNOWN_METHOD => {
            let (report, _) = contract::verify(host, &task.id, workspace, &task.contract, cancel).await?;
            journal.append(kinds::VERIFICATION, json!({"task_id": task.id, "report": report})).await?;
            let undecided_only = !report.ok && report.checks.iter().all(|check| check.ok || check.indeterminate);
            let status = if report.ok {
                "done"
            } else if undecided_only {
                "indeterminate"
            } else {
                "in_progress"
            };
            let _ = host.call(memory_methods::TASK_UPDATE, json!({"id": task.id, "status": status})).await;
            Ok(report)
        }
        Err(error) => Err(error),
    }
}

fn report_text(data: &Dataset, report: &VerifyReport) -> String {
    let mut text = String::new();
    for check in &report.checks {
        let spec = report.contract.checks.iter().find(|spec| spec.name == check.name).map(|spec| spec.spec.as_str()).unwrap_or("");
        let exit = check.exit_code.map(|code| code.to_string()).unwrap_or_else(|| "none".into());
        if check.ok {
            text.push_str(&data.text("check_passed", &[("command", spec)]));
        } else {
            text.push_str(&data.text("check_failed", &[("command", spec), ("exit", &exit), ("output", check.output_tail.trim_end())]));
        }
    }
    text
}

/// Finish the run with an outcome.
fn outcome(outcome: &str, summary: String, turns: u32, tool_calls: u32, changed: &BTreeSet<String>, verified: Option<bool>) -> LoopOutcome {
    LoopOutcome { outcome: outcome.to_owned(), summary, turns, tool_calls, changed_files: changed.iter().cloned().collect(), verified }
}

/// Run the loop until the work is done, blocked, stopped, cancelled or parked.
pub async fn run(env: &RunEnv, journal: &mut Journal, spec: &LoopSpec) -> Result<LoopOutcome, RpcError> {
    repair_interrupted(journal, &env.run_id, &env.data).await?;
    let entries = journal.entries_vec();
    let mut step = next_step(&entries, &env.run_id);
    let mut detector = detector_from(&entries, &env.run_id, env.data.limits.repeat_at);
    // Fix cycles that did not help: the same command still failing after an edit since its last
    // failure (debugging effectiveness decays within 2-3 attempts; a different approach beats a 4th).
    // command -> (edited since its last failure, fruitless cycles, that failure's output)
    let mut failing: HashMap<String, (bool, u32, String)> = HashMap::new();
    // This task's loop changed a file: its steps are fixes from here on (see the thinking budget).
    let mut edited = false;
    let mut fruitless = 0u32;
    // Commands run since a failing command last turned to pass, counted once some command has
    // failed twice since then (a probe that fails once is an answer, not a loop): command -> its
    // failures since that pass.
    let mut stalled = 0u32;
    let mut refailed: HashMap<String, u32> = HashMap::new();
    // The same error coming back ([`crate::typeerrors::error_key`]): its key, the failing runs it
    // came back in with other calls between (any command, any file: edits made through the
    // terminal or in scratch files count too), and whether that was said; a call since the last
    // failing run; declarations shown with it.
    let mut same_error: Option<(u64, u32, bool)> = None;
    let mut worked_since_run = false;
    let mut types_shown = false;
    // What the run was shown under its failures already (`learn::at_failure`): each item once.
    let mut failure_shown: HashSet<String> = of_run(&entries, &env.run_id)
        .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "lesson")
        .flat_map(|entry| ["lessons", "facts"].into_iter().flat_map(|field| entry.data.get(field).and_then(Value::as_array).cloned().unwrap_or_default()))
        .filter_map(|id| id.as_str().map(str::to_owned))
        .collect();
    // A replay without a lesson never shows it.
    if let Some((arm, lesson, _)) = crate::replay::arm(&entries)
        && arm == "without"
    {
        failure_shown.insert(lesson);
    }
    let mut changed = changed_files_of(&entries, &env.run_id);
    let mut tracker = Tracker::default();
    let mut effort = spec.effort.clone();
    // The contract can be revised during the run (a check found broken in itself).
    let mut task = spec.task.clone();
    // A failing contract raised the effort earlier in this run.
    if of_run(&entries, &env.run_id).any(|entry| entry.kind == kinds::NOTE && matches!(str_field(&entry.data, "purpose"), "verify" | "repeat")) {
        effort = max_effort(&effort, &env.data.effort.escalate);
    }
    // The last test command found no tests (`display.no_tests`): a "done" after it is checked by
    // nothing, and is answered once with the step that makes it a check (seen: a fix "verified" by
    // a jest run that found no tests, and the same failure handed back).
    let mut checked_nothing = false;
    let mut told_checked_nothing = false;
    // A project file changed after the last passing test run: a "done" then rests on code no test
    // ran, and is answered once with the run that checks it (seen: a fix edited after the green run
    // and handed back failing).
    let mut ran_tests = false;
    let mut edited_after_pass = false;
    let mut told_edited_after_pass = false;
    // The last test command that passed after the last change, and how many tests it ran: at
    // "done" the host runs it once on the code as it was before the turn; tests that pass there too
    // do not check the change (seen: tests that passed on wrong code, the fix taken as done).
    let mut last_pass: Option<(String, u64)> = None;
    let mut baseline_checked = false;
    // An agent works beside the others of its tree: it hears what bears on its task from them.
    let agent_team = match (&spec.kind, &spec.task) {
        (LoopKind::Agent { .. }, Some(task)) => Some(Team::of(env, task).await),
        _ => None,
    };
    let (stage, live) = match &spec.kind {
        // A planned change: the loop is told its step (the plan itself is in the conversation).
        LoopKind::Foreground => ("DO", spec.step.as_ref().map(|step| step_live(&env.data, step)).unwrap_or_default()),
        // A turn continued because a skill's tool reports open work (`flow::work_open`): the
        // stage's last line says the next step is the one the tool printed, so the ASK rules'
        // "reply when you know the answer" does not win over it (seen: the model answered with a
        // progress report at "Current stage: ASK" while the tool had 168 lines open).
        LoopKind::Question => ("ASK", if crate::flow::work_open_round(&entries, &env.run_id) { env.data.text("work_open_stage", &[]) } else { String::new() }),
        LoopKind::Agent { name } => ("TASK", format!("{}{}", agent_team.as_ref().map(|team| team.live(&env.data)).unwrap_or_default(), env.data.text("agent_live", &[("name", name)]))),
    };
    env.set_stage("act");
    let mut turns = 0u32;
    let mut tool_calls = 0u32;
    let any = env.stops.any();
    // Finding out is a step the correction can name while the research plugin is active.
    let research = research_available(env).await;
    // Approvals a predecessor was waiting for when it stopped.
    if !resolve_pending_approvals(env, journal).await? {
        return Ok(outcome(if env.stops.park.is_cancelled() { "parked" } else { "cancelled" }, String::new(), turns, tool_calls, &changed, None));
    }
    let conversation = |entries: &[JournalEntry]| crate::prewarm::conversation(&env.prefix, &env.data, entries, env.image_input);
    let prompt = |entries: &[JournalEntry]| {
        let mut messages = conversation(entries);
        messages.push(prompts::stage_message(&env.data, &env.prefix.role, stage, &live));
        messages
    };
    // The compacted conversation being prefilled while the step's tools run (crate::fit::prepare).
    let mut prewarm: Option<crate::fit::Prewarm> = None;
    let mut steps_since_settle = 0u32;
    // An agent's session spoke and the agent has not called a tool since.
    let mut answering_session = false;
    loop {
        if env.stops.park.is_cancelled() {
            return Ok(outcome("parked", String::new(), turns, tool_calls, &changed, None));
        }
        if env.stops.cancel.is_cancelled() {
            return Ok(outcome("cancelled", "Cancelled.".into(), turns, tool_calls, &changed, None));
        }
        journal.sync().await?;
        let steers = deliver_steers(journal).await?;
        if !steers.is_empty() {
            detector.reset();
            answering_session = matches!(spec.kind, LoopKind::Agent { .. });
            // A plan task is one part of a plan: a message during it goes to the planner, which
            // writes the tasks still to do so that they do what the user asks now (seen: "fix all
            // the issues" answered eleven times with "task 1 is complete", the step message saying
            // "do this task only" after it at every step).
            // An agent's messages (the session's, another agent's) are its next step inside the task
            // it works on, not a new plan: replanning at each one restarted its reading every time
            // (seen: sixteen plans and the same file read again after each message).
            if spec.step.is_some() && !matches!(spec.kind, LoopKind::Agent { .. }) {
                return Ok(outcome("steered", steers.join("\n\n"), turns, tool_calls, &changed, None));
            }
        }
        if let LoopKind::Agent { name } = &spec.kind {
            deliver_board(journal, env, name, agent_team.as_ref()).await?;
        }
        // The guide of a language the work just reached its first file of, before the next step.
        crate::guides::deliver(env, journal).await?;
        // A prewarm still running ends here: the call continues from what it prefilled.
        drop(prewarm.take());
        // Every `settle_steps` steps the in-place rewrites apply up to the newest entry (outdated
        // reads, earlier edit echoes, outputs of a command run again): the history the model
        // continues from stays small, at one re-read of that history per settle.
        if env.data.limits.settle_steps > 0 && steps_since_settle >= env.data.limits.settle_steps {
            crate::fit::settle(env, journal, prompt).await?;
            steps_since_settle = 0;
        }
        // The prompt is counted and compacted until it fits the conversation's context (crate::fit).
        let messages = match crate::fit::fitted(env, journal, prompt).await {
            Ok(messages) => messages,
            Err(error) if error.code == crate::fit::CONTEXT_FULL => {
                journal.note(&env.run_id, "context", &error.message, false).await?;
                return Ok(outcome("failed", error.message, turns, tool_calls, &changed, None));
            }
            Err(error) => return Err(error),
        };
        // The step's own entries start here; what is before it is what the next call shares.
        let step_start = journal.last_seq() + 1;
        let mut call = env.call(stage, &effort, messages, None, None, step, true);
        // A plan task is execution: short thinking, so values come from running code, not from
        // simulating it in thought (seen: thousands of characters tracing a loop by hand).
        // Thinking in proportion to the step: the full budget while the task designs and writes
        // its first change, a short one for every fix after that (the design is already in the
        // file; re-deriving it each step was most of the generated tokens).
        // Every step carries a budget: a loop without a plan step (a question, a follow-up) gets
        // the loop budget, never the model plugin's effort table.
        let budget = if spec.step.is_some() {
            if edited && env.data.limits.fix_think_tokens > 0 { env.data.limits.fix_think_tokens } else { env.data.limits.task_think_tokens }
        } else {
            env.data.limits.loop_think_tokens
        };
        if budget > 0 {
            call.think_budget = Some(budget);
        }
        let called = match llm::chat(journal, call, &any).await {
            Ok(called) => {
                steps_since_settle += 1;
                called
            }
            Err(error) if error.code == codes::CANCELLED => continue,
            // The model cannot run the request at all (a prompt larger than its conversation holds,
            // an image it cannot read): nothing the model wrote is wrong, so it is not asked to
            // write again (seen: "tool call arguments must be valid JSON" for an oversized prompt).
            Err(error) if error.code == codes::INVALID_PARAMS => {
                journal.note(&env.run_id, "model_error", &error.message, false).await?;
                return Ok(outcome("failed", error.message, turns, tool_calls, &changed, None));
            }
            // The engine could not use the generation (e.g. tool-call arguments that are not JSON):
            // the model writes it again with the reason; the same failure repeating stops the run.
            Err(error) if !host::refused(&error) && !host::interrupted(&error) => {
                step += 1;
                turns += 1;
                match detector.observe("model_error", &error.code) {
                    Verdict::Stop(_) => return Err(error),
                    Verdict::Replan(_) => {
                        let what = env.data.text("repeated_text", &[]);
                        journal.note(&env.run_id, "repeat", &prompts::repetition_correction(&env.data, &what, research), true).await?;
                        effort = max_effort(&effort, &env.data.effort.escalate);
                    }
                    Verdict::Fine | Verdict::Reread(_) | Verdict::Rerun(_) => {}
                }
                journal.note(&env.run_id, "model_error", &env.data.text("generation_failed", &[("error", &error.message)]), true).await?;
                continue;
            }
            Err(error) => return Err(error),
        };
        step += 1;
        turns += 1;
        let result = called.result;

        if result.finish_reason == "repeated" {
            // The model plugin stopped the generation for repeating itself; its text stands but is
            // not acted on.
            if let Verdict::Stop(_) = detector.generation_repeated() {
                let summary = "The work stopped because the model kept repeating itself after a corrective replan.".to_owned();
                journal.note(&env.run_id, "repeat", &summary, false).await?;
                return Ok(outcome("stopped", summary, turns, tool_calls, &changed, None));
            }
            journal.note(&env.run_id, "repeat", &prompts::repetition_correction(&env.data, &env.data.text("repeated_text", &[]), research), true).await?;
            effort = max_effort(&effort, &env.data.effort.escalate);
            continue;
        }

        if result.tool_calls.is_empty() {
            if result.finish_reason == "length" && result.content.trim().is_empty() {
                if crate::fit::compact_now(env, journal, prompt).await? {
                    continue;
                }
                let summary = "The conversation no longer fits the model's context window.".to_owned();
                journal.note(&env.run_id, "context", &summary, false).await?;
                return Ok(outcome("failed", summary, turns, tool_calls, &changed, None));
            }
            journal.sync().await?;
            if !journal.pending_steers("steer").is_empty() {
                continue;
            }
            let summary = result.content.trim().to_owned();
            // An agent that answered its session's message in words and ended its turn does what
            // the session said: the run ends here, and the merge steward judges the work. The
            // run's own checks would put it back to work against the session's word (seen: "task 4
            // is done, stop" answered "acknowledged", then the failing checks of another task's
            // files sent it on for twenty more steps).
            if answering_session {
                return Ok(outcome("done", summary, turns, tool_calls, &changed, None));
            }
            if checked_nothing && !told_checked_nothing {
                told_checked_nothing = true;
                journal.note(&env.run_id, "no_tests", &env.data.text("done_without_tests", &[]), true).await?;
                continue;
            }
            if ran_tests && edited_after_pass && !told_edited_after_pass {
                told_edited_after_pass = true;
                journal.note(&env.run_id, "unchecked_edit", &env.data.text("done_after_edit", &[]), true).await?;
                continue;
            }
            if let Some((command, ran)) = last_pass.clone().filter(|_| !baseline_checked && !edited_after_pass) {
                baseline_checked = true;
                env.set_stage("verify");
                let params = json!({"session_id": env.session_id, "workspace": env.scope.workspace, "command": command});
                let result = env.host.call_with(tools_methods::TESTS_BASELINE, params, crate::host::Effect::Effect, Some(&any)).await;
                env.set_stage("act");
                match result {
                    Err(error) if error.code == codes::CANCELLED => continue,
                    Ok(result) if baseline_passed(&result, ran) => {
                        journal.note(&env.run_id, "tests_pass_before", &env.data.text("done_tests_pass_before", &[("command", &command)]), true).await?;
                        continue;
                    }
                    _ => {}
                }
            }
            let Some(task) = task.as_mut().filter(|task| !task.contract.checks.is_empty()) else {
                return Ok(outcome("done", summary, turns, tool_calls, &changed, None));
            };
            env.set_stage("verify");
            let mut report = match verify_task(&env.host, journal, task, &env.scope.workspace, &any).await {
                Ok(report) => report,
                Err(error) if error.code == codes::CANCELLED => continue,
                Err(error) => return Err(error),
            };
            let fixable = |report: &VerifyReport| report.checks.iter().any(|check| !check.ok && !check.indeterminate);
            if fixable(&report) {
                // A check can turn out to be broken in itself only now. The worker may only propose
                // a revision; the planner accepts or refuses it (the contract is frozen).
                let proposer = match &spec.kind {
                    LoopKind::Agent { name } => name.clone(),
                    _ => format!("session:{}", env.session_id),
                };
                let proposal = match crate::checks::propose_after_failure(env, journal, &task.contract, &report).await {
                    Ok(proposal) => proposal,
                    Err(error) if error.code == codes::CANCELLED => continue,
                    Err(error) => return Err(error),
                };
                if let Some((proposed, evidence)) = proposal {
                    let in_force = match crate::checks::submit(env, journal, task, &proposed, &proposer, &evidence, "after").await {
                        Ok(contract) => contract,
                        Err(error) if error.code == codes::CANCELLED => continue,
                        Err(error) => return Err(error),
                    };
                    if in_force != task.contract {
                        task.contract = in_force;
                        env.set_stage("verify");
                        report = match verify_task(&env.host, journal, task, &env.scope.workspace, &any).await {
                            Ok(report) => report,
                            Err(error) if error.code == codes::CANCELLED => continue,
                            Err(error) => return Err(error),
                        };
                    }
                }
            }
            if !report.ok && !fixable(&report) {
                // What is left the host cannot decide (manual checks): INDETERMINATE, for a person.
                let undecided: String = report
                    .checks
                    .iter()
                    .filter(|check| check.indeterminate)
                    .map(|check| format!("\n- {}", check.output_tail.trim()))
                    .collect();
                let note = env.data.text("checks_indeterminate", &[("checks", &undecided)]);
                journal.note(&env.run_id, "verify", &note, false).await?;
                return Ok(outcome("blocked", format!("{summary}\n\n{note}").trim().to_owned(), turns, tool_calls, &changed, None));
            }
            env.set_stage("act");
            if report.ok {
                return Ok(outcome("done", summary, turns, tool_calls, &changed, Some(true)));
            }
            // Only tests that failed before the work fail, and a later task waits for this one
            // (the plan's last task fixes what failed before): this task is done.
            // Or its errors are all outside what it changed (another task's part, seen: one agent's
            // module removal failing every other agent's build check for hours).
            let own: std::collections::HashSet<String> = changed.iter().map(|path| crate::impact::relative(&env.scope, path)).chain(tracker.reached.iter().cloned()).collect();
            let awaited = crate::checks::awaited(env, task).await;
            let not_its = awaited
                && !crate::checks::no_worse(journal, &report)
                && match contract::verify(&env.host, &task.id, &env.scope.workspace, &task.contract, &any).await {
                    Ok((full, outputs)) => !full.ok && crate::checks::not_its_failures(&full, &outputs, &own, &env.scope.workspace),
                    Err(error) if error.code == codes::CANCELLED => continue,
                    Err(_) => false,
                };
            if awaited && (not_its || crate::checks::no_worse(journal, &report)) {
                journal.note(&env.run_id, "verify", &env.data.text("checks_no_worse", &[]), false).await?;
                // In the project itself it is done now; an agent's workspace waits for the merge
                // steward, which judges it the same way after merging.
                if env.scope.workspace == env.scope.root {
                    let _ = env.host.call(memory_methods::TASK_UPDATE, json!({"id": task.id, "status": "done"})).await;
                }
                return Ok(outcome("done", summary, turns, tool_calls, &changed, Some(true)));
            }
            let failures = report_text(&env.data, &report);
            match detector.observe("contract", &failures) {
                Verdict::Stop(_) => {
                    let summary = format!("The contract kept failing the same way after a corrective replan:{failures}");
                    journal.note(&env.run_id, "repeat", &summary, false).await?;
                    return Ok(outcome("stopped", summary, turns, tool_calls, &changed, Some(false)));
                }
                Verdict::Replan(_) => {
                    journal.note(&env.run_id, "repeat", &prompts::repetition_correction(&env.data, &env.data.text("repeated_checks", &[]), research), true).await?;
                }
                Verdict::Fine | Verdict::Reread(_) | Verdict::Rerun(_) => {}
            }
            let failures = format!("{failures}{}", crate::checks::failed_before(&env.data, journal, &env.run_id, &report));
            journal.note(&env.run_id, "verify", &prompts::contract_failure(&env.data, &failures), true).await?;
            effort = max_effort(&effort, &env.data.effort.escalate);
            continue;
        }

        answering_session = false;
        // Journal the model's calls as written, then execute them in order.
        let mut call_seqs = Vec::new();
        for call in &result.tool_calls {
            let args: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| Value::String(call.arguments.clone()));
            let seq = journal
                .append(
                    kinds::TOOL_CALL,
                    json!({
                        "call_id": call.id,
                        "tool": call.name,
                        "intent": args.get("i").or_else(|| args.get("intent")).and_then(Value::as_str).unwrap_or(""),
                        "args": args,
                        "raw_name": call.name,
                        "raw_arguments": call.arguments,
                        "run_id": env.run_id,
                        "step": step - 1,
                    }),
                )
                .await?;
            call_seqs.push(seq);
        }
        // The tools take a while: compact now if the conversation is getting long, and let the
        // model re-read it meanwhile.
        prewarm = crate::fit::prepare(env, journal, step_start, env.data.limits.compact_early_tokens, crate::prewarm::When::Soon, conversation).await;
        let mut verdicts = Vec::new();
        let mut stopped = false;
        for (index, call) in result.tool_calls.iter().enumerate() {
            if index > 0 {
                journal.sync().await?;
            }
            // A waiting message skips the calls that would change something; reads already sent run,
            // so the model does not send them again after the message (seen: 46 reads skipped and
            // re-sent in one run). An agent's calls all run and its session's message comes with
            // their results: skipping them threw away edits it had decided (seen: two prepared edits
            // lost twice to the session's "keep going" messages). A stop reaches it as a real stop.
            let steer_waiting = index > 0 && call.name != "read" && !matches!(spec.kind, LoopKind::Agent { .. }) && !journal.pending_steers("steer").is_empty();
            if stopped || steer_waiting || env.stops.park.is_cancelled() || env.stops.cancel.is_cancelled() {
                let why = if steer_waiting { env.data.text("skipped_steer", &[]) } else { env.data.text("skipped_stopped", &[]) };
                journal
                    .append(kinds::TOOL_RESULT, json!({"call_id": call.id, "ok": false, "output": why, "duration_ms": 0, "run_id": env.run_id, "skipped": true}))
                    .await?;
                continue;
            }
            match execute_call(env, journal, Some(call_seqs[index]), call).await {
                Exec::Stopped => {
                    stopped = true;
                    let output = if env.stops.park.is_cancelled() { env.data.text("interrupted", &[]) } else { env.data.text("cancelled_running", &[]) };
                    journal
                        .append(kinds::TOOL_RESULT, json!({"call_id": call.id, "ok": false, "output": output, "duration_ms": 0, "run_id": env.run_id, "interrupted_call": true}))
                        .await?;
                }
                Exec::Failed(message) => {
                    tool_calls += 1;
                    journal
                        .append(kinds::TOOL_RESULT, json!({"call_id": call.id, "ok": false, "output": message, "duration_ms": 0, "run_id": env.run_id}))
                        .await?;
                    verdicts.push(detector.observe_call(&call.name, &serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::Null), &message));
                }
                Exec::Done(mut outcome) => {
                    // Ask mode asks for every risky call; an approval the tools mark `always` (a command
                    // the model asked to run past the sandbox) is asked for in every mode.
                    let always = outcome.display.pointer("/approval/always").and_then(Value::as_bool).unwrap_or(false);
                    if outcome.needs_approval && (env.mode == "ask" || always) {
                        match await_approval(env, journal, call, &outcome, None).await? {
                            Approval::Approved(approved) => outcome = approved,
                            Approval::Answered(text) => {
                                tool_calls += 1;
                                journal
                                    .append(kinds::TOOL_RESULT, json!({"call_id": call.id, "ok": false, "output": text, "duration_ms": 0, "run_id": env.run_id}))
                                    .await?;
                                continue;
                            }
                            Approval::Stopped => {
                                stopped = true;
                                continue;
                            }
                        }
                    }
                    tool_calls += 1;
                    let seq = call_seqs[index];
                    // Record the repaired, normalized form of the call for the transcript.
                    journal.set(seq, "tool", json!(outcome.tool)).await?;
                    journal.set(seq, "args", outcome.args.clone()).await?;
                    if !outcome.intent.is_empty() {
                        journal.set(seq, "intent", json!(outcome.intent)).await?;
                    }
                    let mut data = json!({
                        "call_id": call.id,
                        "ok": outcome.ok,
                        "output": outcome.output,
                        "display": outcome.display,
                        "duration_ms": outcome.duration_ms,
                        "changed_files": outcome.changed_files,
                        "run_id": env.run_id,
                    });
                    if let Some(artifact) = &outcome.artifact {
                        data["artifact"] = json!(artifact);
                    }
                    if let Some(kind) = &outcome.error_kind {
                        data["error_kind"] = json!(kind);
                    }
                    if outcome.needs_approval {
                        data["needs_approval"] = json!(true);
                    }
                    // A failing run: the declarations of the project's types its errors go through,
                    // right under the error, where the next fix is decided (the line number alone
                    // pulls every fix to that line).
                    // Failed by its exit status, or by the error report a pipe hid behind status 0.
                    let run_failed = if outcome.tool == "run" { outcome.display.get("exit_code").and_then(Value::as_i64).map(|code| crate::typeerrors::failed(code, &outcome.output)) } else { None };
                    if run_failed == Some(true) {
                        let declared = crate::typeerrors::declarations(&env.host, &env.scope, &outcome.output).await;
                        types_shown = declared.is_some();
                        if let Some(declared) = declared {
                            data["output"] = json!(format!("{}\n\n{}", outcome.output.trim_end(), env.data.text("error_types", &[("items", &declared)])));
                        }
                    }
                    // A failing test (a test report in the result) repeats for another reason than a
                    // compile error: its note speaks of the test, not of a declaration.
                    let test_run = data.get("display").and_then(|display| display.get("tests")).is_some_and(|tests| !tests.is_null());
                    // A read that shows exactly what an earlier read still in the conversation shows
                    // (the file unchanged since): a pointer to it instead of the text again (seen: a
                    // third of a run's reads were files it had already read, a fifth of what it read).
                    if outcome.tool == "read" && outcome.ok && outcome.output.len() >= UNCHANGED_MIN_CHARS {
                        let entries = journal.entries_vec();
                        let earlier = entries.iter().rev().find(|entry| entry.kind == kinds::TOOL_RESULT && str_field(&entry.data, "output") == outcome.output).map(|entry| entry.seq);
                        if let Some(earlier) = earlier
                            && crate::projection::project(&entries, &env.data).iter().any(|message| message.content.contains(outcome.output.as_str()))
                        {
                            let path = outcome.args.get("path").and_then(Value::as_str).unwrap_or("the file");
                            data["output"] = json!(env.data.text("read_unchanged", &[("path", path)]));
                            data["unchanged"] = json!(true);
                            data["repeat_of"] = json!(earlier);
                        }
                    }
                    // A range of a file whose lines an earlier read still in view shows, the file
                    // unchanged since: a pointer to that read (seen: one file read in fifteen ranges
                    // after a whole read, some ranges twice).
                    if outcome.tool == "read" && outcome.ok && data.get("unchanged").is_none() {
                        let entries = journal.entries_vec();
                        let path = read_target(&outcome.args);
                        if let Some((earlier, earlier_output)) = covered_by_earlier_read(&entries, &path)
                            && crate::projection::project(&entries, &env.data).iter().any(|message| message.content.contains(earlier_output.as_str()))
                        {
                            data["output"] = json!(env.data.text("read_in_view", &[]));
                            data["unchanged"] = json!(true);
                            data["repeat_of"] = json!(earlier);
                        }
                    }
                    journal.append(kinds::TOOL_RESULT, data).await?;
                    // A failure something was learned from before: what worked then, right under it,
                    // where the next step is decided.
                    // A refused edit's own message shows the current lines and what to send instead: that is
                    // the step, and a lesson under it took its place (seen: "re-read the file to get fresh
                    // anchors" under a refusal that showed them, then four reads).
                    let refusal = outcome.error_kind.as_deref().filter(|kind| !outcome.ok && !kind.is_empty() && *kind != "nonzero_exit" && outcome.tool != "edit");
                    let failure = match (run_failed, refusal) {
                        (_, Some(kind)) => crate::learn::failure_key(&outcome.tool, kind, ""),
                        (Some(true), None) => crate::learn::failure_key("run", "", &outcome.output),
                        _ => None,
                    };
                    if let Some(key) = failure {
                        failure_note(env, journal, &key, &mut failure_shown).await?;
                    }
                    if run_failed != Some(true) {
                        worked_since_run = true;
                    }
                    match run_failed {
                        Some(false) => same_error = None,
                        Some(true) => {
                            let worked = worked_since_run;
                            let key = crate::typeerrors::error_key(&outcome.output);
                            match (key, &mut same_error) {
                                (Some(key), Some((known, survived, _))) if *known == key => {
                                    if worked {
                                        *survived += 1;
                                    }
                                }
                                (Some(key), _) => same_error = Some((key, 0, false)),
                                (None, _) => {}
                            }
                            worked_since_run = false;
                            let limit = env.data.limits.same_error_edits;
                            if let Some((_, survived, said)) = &mut same_error
                                && limit > 0
                                && *survived >= limit
                                && !*said
                            {
                                *said = true;
                                let key = if test_run {
                                    "same_error_test"
                                } else if types_shown {
                                    "same_error"
                                } else {
                                    "same_error_plain"
                                };
                                journal.note(&env.run_id, "same_error", &env.data.text(key, &[("edits", &survived.to_string())]), true).await?;
                            }
                        }
                        None => {}
                    }
                    if outcome.tool == "read"
                        && outcome.ok
                        && let Some(path) = outcome.args.get("path").or_else(|| outcome.args.get("target")).and_then(Value::as_str)
                        && !path.contains("://")
                    {
                        tracker.remember(&env.host, &env.scope, path).await;
                    }
                    if !outcome.changed_files.is_empty() {
                        edited = true;
                        changed.extend(outcome.changed_files.iter().cloned());
                        if let Some(text) = tracker.after_edit(&env.host, &env.scope, &outcome.changed_files, &env.data).await {
                            let seq = journal.note(&env.run_id, "impact", &text, true).await?;
                            // The merge steward reads which files this run's changes reached.
                            journal.set(seq, "reached", json!(tracker.reached.iter().collect::<Vec<_>>())).await?;
                            // The agents whose tasks change the code this reached hear it at once,
                            // with what changed (their next board delivery shows it).
                            if let (LoopKind::Agent { name }, Some(team)) = (&spec.kind, agent_team.as_ref()) {
                                tell_reached(env, name, team, &tracker.reached, &text).await;
                            }
                        }
                    }
                    if !outcome.changed_files.is_empty() {
                        for (edited, _, _) in failing.values_mut() {
                            *edited = true;
                        }
                    }
                    // The model's own change of a project file (a one-off try in `tmp/` is none).
                    if outcome.tool == "edit" && outcome.changed_files.iter().any(|path| !path.starts_with("tmp/") && !path.contains("/tmp/")) {
                        edited_after_pass = true;
                        baseline_checked = false;
                    }
                    // A refused or unrun command (`run_failed == None`) is not a run: it counts for
                    // nothing here. A recognised test run (`display.tests`, see the tools plugin's
                    // test view) is what the counters watch: a green one resets them, a failing one
                    // opens them. A probe, an install or a search never does either (seen: a task
                    // with 16 green tests ended as stuck because a denied install and a probe that
                    // exits 1 by design "never turned to pass").
                    if outcome.tool == "run"
                        && run_failed.is_some()
                        && let Some(command) = outcome.args.get("command").and_then(Value::as_str)
                    {
                        let tests = outcome.display.get("tests").filter(|tests| tests.is_object());
                        let failed_tests = tests.and_then(|tests| tests.get("failed")).and_then(Value::as_u64);
                        if outcome.display.get("no_tests").and_then(Value::as_bool) == Some(true) {
                            checked_nothing = true;
                        } else if tests.is_some() {
                            checked_nothing = false;
                            ran_tests = true;
                            if run_failed == Some(false) {
                                edited_after_pass = false;
                                let ran = tests.and_then(|tests| tests.get("ran")).and_then(Value::as_u64).unwrap_or(0);
                                last_pass = Some((command.to_owned(), ran));
                            }
                        }
                        if tests.is_some() && run_failed == Some(false) {
                            stalled = 0;
                            refailed.clear();
                            failing.clear();
                        } else {
                            if tests.is_some() && run_failed == Some(true) {
                                *refailed.entry(command.to_owned()).or_default() += 1;
                            }
                            if refailed.values().any(|&failures| failures >= 2) {
                                stalled += 1;
                            }
                        }
                        match run_failed {
                            Some(false) => {
                                failing.remove(command);
                            }
                            Some(true) => {
                                // Fruitless only when an edit left the very same failure: a fix that
                                // changed what fails is progress (fewer or other failing cases). For
                                // a test run the failing count is the measure; otherwise the same
                                // error lines without numbers (moved lines and counters do not make
                                // it new), else the same output.
                                let same = match failed_tests {
                                    Some(failed) => format!("tests:{failed}"),
                                    None => crate::typeerrors::error_key(&outcome.output).map(|key| key.to_string()).unwrap_or_else(|| outcome.output.clone()),
                                };
                                let entry = failing.entry(command.to_owned()).or_insert((false, 0, String::new()));
                                if entry.0 {
                                    if entry.2 == same {
                                        entry.1 += 1;
                                        fruitless = fruitless.max(entry.1);
                                    } else {
                                        entry.1 = 0;
                                    }
                                }
                                entry.0 = false;
                                entry.2 = same;
                            }
                            None => {}
                        }
                    }
                    verdicts.push(detector.observe_call(&outcome.tool, &outcome.args, &outcome.output));
                    if !outcome.changed_files.is_empty() {
                        detector.changed();
                    }
                }
            }
        }
        if stopped {
            continue;
        }
        if verdicts.iter().any(|verdict| matches!(verdict, Verdict::Stop(_))) {
            let what = verdicts.iter().find_map(|verdict| if let Verdict::Stop(what) = verdict { Some(what.clone()) } else { None }).unwrap_or_default();
            let summary = format!("The work stopped: the agent kept repeating {what} with the same result after a corrective replan.");
            journal.note(&env.run_id, "repeat", &summary, false).await?;
            return Ok(outcome("stopped", summary, turns, tool_calls, &changed, None));
        }
        if let Some(Verdict::Replan(what)) = verdicts.iter().find(|verdict| matches!(verdict, Verdict::Replan(_))) {
            journal.note(&env.run_id, "repeat", &prompts::repetition_correction(&env.data, what, research), true).await?;
            effort = max_effort(&effort, &env.data.effort.escalate);
        } else if let Some(Verdict::Reread(what)) = verdicts.iter().find(|verdict| matches!(verdict, Verdict::Reread(_))) {
            // A reread is harmless: said once, plainly, with the step to take.
            journal.note(&env.run_id, "repeat", &env.data.text("reread", &[("what", what)]), true).await?;
        } else if let Some(Verdict::Rerun(what)) = verdicts.iter().find(|verdict| matches!(verdict, Verdict::Rerun(_))) {
            // A passing check run again with nothing changed: the step is to finish.
            journal.note(&env.run_id, "repeat", &env.data.text("rerun", &[("what", what)]), true).await?;
        }
        // A plan task that used its calls without finishing is stuck: it ends here and the flow
        // writes the tasks still to do again with another approach, from where the model says it is.
        let cycles = env.data.limits.fix_cycles;
        let stall = env.data.limits.stall_runs;
        let fixes_failed = cycles > 0 && fruitless >= cycles;
        let runs_stalled = stall > 0 && stalled >= stall;
        if spec.step.is_some() && ((env.data.limits.task_calls > 0 && tool_calls >= env.data.limits.task_calls) || fixes_failed || runs_stalled) {
            let last_words = journal
                .entries()
                .filter(|entry| entry.kind == kinds::ASSISTANT_MESSAGE && entry.data.get("run_id").and_then(Value::as_str) == Some(env.run_id.as_str()))
                .filter_map(|entry| entry.data.get("text").and_then(Value::as_str).map(str::to_owned))
                .last()
                .unwrap_or_default();
            // Said as what stopped it: the fix cycles that did not help, else the calls it used.
            let key = if fixes_failed {
                "task_stuck_fixes"
            } else if runs_stalled {
                "task_stuck_runs"
            } else {
                "task_stuck"
            };
            let summary = env.data.text(
                key,
                &[("calls", &tool_calls.to_string()), ("fixes", &fruitless.to_string()), ("runs", &stalled.to_string()), ("last", last_words.trim())],
            );
            journal.note(&env.run_id, "task_stuck", &summary, false).await?;
            return Ok(outcome("stuck", summary, turns, tool_calls, &changed, None));
        }
    }
}

/// Whether the baseline run (the last passing test command on the code before the turn) passed as
/// well: it ran at least as many tests as the passing run (a test inside a code file put back is
/// gone there, so fewer tests decide nothing), none failed, and it finished in time.
fn baseline_passed(result: &Value, ran: u64) -> bool {
    let number = |field: &str| result.get(field).and_then(Value::as_u64);
    result.get("checked").and_then(Value::as_bool) == Some(true)
        && result.get("timed_out").and_then(Value::as_bool) != Some(true)
        && ran > 0
        && number("ran").is_some_and(|before| before >= ran)
        && number("failed") == Some(0)
}
