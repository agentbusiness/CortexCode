//! A session controls the background agents it started: MANAGE (the user's word, or a wake when
//! one of them needs a decision) stops, restarts or messages them and may plan the work again; a
//! plan names the open work it replaces, and that work stops when the plan starts.
//!
//! The session's work is structural: the agents its journal records as spawned (`agent_spawned`),
//! with their records and tasks as the agents and memory plugins hold them now. The model only
//! picks among those names; the host carries the picks out.

use crate::agent_loop::RunEnv;
use crate::journal::Journal;
use crate::schemas::{self, Manage};
use cc_protocol::RpcError;
use cc_protocol::contracts::agents::{CancelResult, ListResult, WorkersResult, events, methods as agents_methods, workers};
use cc_protocol::contracts::chatflow::{ManageFlag, ManageFlags, topics as chatflow_topics};
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::memory::{AgentRecord, Task, methods as memory_methods};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Characters of a task's last result shown per agent.
const RESULT_CHARS: usize = 400;

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

/// One agent of this session whose task is still open.
#[derive(Debug, Clone)]
pub struct OpenWork {
    pub agent: AgentRecord,
    pub task: Task,
    /// It failed the same way twice: a restart is the user's call.
    pub repeated: bool,
}

impl OpenWork {
    /// How a plan's `replaces` names it.
    pub fn label(&self) -> String {
        format!("{}: {}", self.agent.name, self.task.title)
    }
}

/// The agents this session started whose task is neither done nor cancelled, oldest first.
pub async fn open_work(env: &RunEnv, entries: &[JournalEntry]) -> Vec<OpenWork> {
    let mut spawned: Vec<(String, String)> = Vec::new();
    for entry in entries.iter().filter(|entry| entry.kind == kinds::AGENT_SPAWNED) {
        let (agent, task_id) = (str_field(&entry.data, "agent").to_owned(), str_field(&entry.data, "task_id").to_owned());
        spawned.retain(|(known, _)| *known != agent);
        if !agent.is_empty() && !task_id.is_empty() {
            spawned.push((agent, task_id));
        }
    }
    if spawned.is_empty() {
        return Vec::new();
    }
    let Ok(list) = env.host.get::<ListResult>(agents_methods::LIST, json!({"project_id": env.scope.project_id})).await else { return Vec::new() };
    let records: HashMap<String, AgentRecord> = list.agents.into_iter().map(|record| (record.name.clone(), record)).collect();
    let mut open = Vec::new();
    for (name, task_id) in spawned {
        let Some(record) = records.get(&name) else { continue };
        if record.status == "done" || record.task_id.as_deref() != Some(task_id.as_str()) {
            continue;
        }
        let Ok(task) = env.host.get::<Task>(memory_methods::TASK_GET, json!({"id": task_id})).await else { continue };
        if matches!(task.status.as_str(), "done" | "cancelled") {
            continue;
        }
        let repeated = repeated_failure(entries, &task.id);
        open.push(OpenWork { agent: record.clone(), task, repeated });
    }
    open
}

/// The task's last two blocks (supervision notes) failed the same way: the same failing tests
/// when both runs named them, else the same text.
pub fn repeated_failure(entries: &[JournalEntry], task_id: &str) -> bool {
    let blocks: Vec<&Value> = entries
        .iter()
        .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "supervise")
        .filter_map(|entry| entry.data.get("supervise"))
        .filter(|event| str_field(event, "event") == events::BLOCKED && str_field(event, "task_id") == task_id)
        .collect();
    let [.., before, last] = blocks.as_slice() else { return false };
    let tests = |event: &Value| event.get("failed_tests").and_then(Value::as_array).filter(|tests| !tests.is_empty()).cloned();
    match (tests(before), tests(last)) {
        (Some(a), Some(b)) => a == b,
        _ => str_field(before, "text").trim() == str_field(last, "text").trim(),
    }
}

/// What an agent's current run did, from its journal (structural facts, no model): steps,
/// edits and the files they changed, reads and check runs since its last edit, and how long ago it
/// last moved. The host names what needs the session's attention: reads without an edit past
/// `limits.manage_reading_calls`, or no step for `limits.manage_idle_minutes` (waiting for the
/// model); the flag is true when one of them holds and the session has not told it anything since
/// its last edit. A run of one of `checks` (its task's check commands, by the command they run) is a
/// check run, not reading (seen: an agent named reading while it ran its checks between edits).
pub async fn progress(env: &RunEnv, agent: &AgentRecord, checks: &[String]) -> (String, bool) {
    let Some((entries, measured)) = measured(env, agent, checks).await else { return (String::new(), false) };
    let Measured { steps, edits, since_edit, checks_since, idle, edited_at, files } = measured;
    let told = entries.iter().rev().find(|entry| entry.kind == kinds::STEER);
    let measured_files = files.clone();
    let files = if files.is_empty() { env.data.text("manage_no_files", &[]) } else { files.join(", ") };
    let mut line = env.data.text(
        "manage_progress",
        &[("steps", &steps.to_string()), ("edits", &edits.to_string()), ("files", &files), ("since", &since_edit.to_string()), ("checks", &checks_since.to_string()), ("idle", &idle.to_string())],
    );
    // What the folder holds now, not what the agent last said: a file it changed that is gone
    // (another agent removed it, or it did) is named (seen: three agents steered to a folder
    // another agent had deleted, from reports written before it went).
    let folder = agent.workspace.as_deref().filter(|workspace| !workspace.is_empty()).unwrap_or(env.scope.workspace.as_str());
    let gone: Vec<&str> = measured_files.iter().map(String::as_str).filter(|file| !std::path::Path::new(folder).join(file).exists()).collect();
    if !gone.is_empty() {
        line.push_str(&env.data.text("manage_files_gone", &[("files", &gone.join(", "))]));
    }
    let (reading, still) = flags(env, since_edit, idle);
    if reading {
        line.push_str(&env.data.text("manage_flag_reading", &[("since", &since_edit.to_string())]));
    }
    if still {
        line.push_str(&env.data.text("manage_flag_idle", &[("idle", &idle.to_string())]));
    }
    // Told something it has not acted on with an edit yet: it is working on that.
    let told_since_edit = told.is_some_and(|told| told.ts_ms > edited_at);
    // Both ways: what the session last told it, and what it last said (its answer, its question).
    if let Some(told) = told {
        let ago = cc_protocol::contracts::now_ms().saturating_sub(told.ts_ms) / 60_000;
        line.push_str(&env.data.text("manage_told", &[("ago", &ago.to_string()), ("text", &short(str_field(&told.data, "text"), SAID_CHARS))]));
    }
    if let Some(said) = entries.iter().rev().find(|entry| entry.kind == kinds::ASSISTANT_MESSAGE && !str_field(&entry.data, "text").trim().is_empty()) {
        line.push_str(&env.data.text("manage_said", &[("text", &short(str_field(&said.data, "text"), SAID_CHARS))]));
    }
    (line, (reading || still) && !told_since_edit)
}

/// What an agent's current run did, counted from its journal ([`progress`] words it).
struct Measured {
    steps: usize,
    edits: usize,
    /// Successful calls that were neither edits nor check runs since the last edit.
    since_edit: usize,
    checks_since: usize,
    /// Minutes since the run's last entry.
    idle: u64,
    edited_at: u64,
    files: Vec<String>,
}

/// The two things that need the session: reading past `limits.manage_reading_calls`, no step for
/// `limits.manage_idle_minutes`.
fn flags(env: &RunEnv, since_edit: usize, idle: u64) -> (bool, bool) {
    let limits = &env.data.limits;
    (limits.manage_reading_calls > 0 && since_edit >= limits.manage_reading_calls as usize, limits.manage_idle_minutes > 0 && idle >= limits.manage_idle_minutes)
}

/// The agent's journal and the counts of its current run; None when it has no run yet.
async fn measured(env: &RunEnv, agent: &AgentRecord, checks: &[String]) -> Option<(Vec<JournalEntry>, Measured)> {
    let session_id = agent.session_id.as_deref()?;
    let journal = Journal::load(env.host.clone(), session_id).await.ok()?;
    let entries = journal.entries_vec();
    let run_id = entries.iter().rev().find(|entry| entry.kind == kinds::STATUS).map(|entry| str_field(&entry.data, "run_id").to_owned())?;
    let check_bases: Vec<String> = checks.iter().map(|command| crate::repetition::base_command(command)).filter(|base| !base.is_empty()).collect();
    let run: Vec<&JournalEntry> = entries.iter().filter(|entry| str_field(&entry.data, "run_id") == run_id).collect();
    let ok: HashMap<&str, bool> = run
        .iter()
        .filter(|entry| entry.kind == kinds::TOOL_RESULT)
        .map(|entry| (str_field(&entry.data, "call_id"), entry.data.get("ok").and_then(Value::as_bool).unwrap_or(false)))
        .collect();
    let steps = run.iter().filter(|entry| entry.kind == kinds::USAGE).count();
    // The session's last message to it: calls are counted from it too, so an agent is not told
    // again before it could act on what it was told (seen: one agent told the same thing five
    // times in a minute, and "hold" and "write it now" in turns every few minutes).
    let told_at = entries.iter().rev().find(|entry| entry.kind == kinds::STEER).map(|entry| entry.ts_ms).unwrap_or(0);
    let mut edits = 0;
    let mut since_edit = 0;
    let mut checks_since = 0;
    let mut edited_at = 0;
    let mut files: Vec<String> = Vec::new();
    for entry in run.iter().filter(|entry| entry.kind == kinds::TOOL_CALL && entry.data.get("host").and_then(Value::as_bool) != Some(true)) {
        let edited = str_field(&entry.data, "tool") == "edit" && ok.get(str_field(&entry.data, "call_id")).copied().unwrap_or(false);
        if edited {
            edits += 1;
            since_edit = 0;
            checks_since = 0;
            edited_at = entry.ts_ms;
            let path = entry.data.get("args").and_then(|args| args.get("path")).and_then(Value::as_str).unwrap_or("");
            if !path.is_empty() && !files.iter().any(|known| known == path) {
                files.push(path.to_owned());
            }
        // Calls that did not run (skipped for a message, an edit refused by another agent's lock)
        // are not reading.
        } else if entry.ts_ms >= told_at && ok.get(str_field(&entry.data, "call_id")).copied().unwrap_or(false) {
            let command = entry.data.get("args").and_then(|args| args.get("command")).and_then(Value::as_str).unwrap_or("");
            let base = crate::repetition::base_command(command);
            if str_field(&entry.data, "tool") == "run" && !base.is_empty() && check_bases.contains(&base) {
                checks_since += 1;
            } else {
                since_edit += 1;
            }
        }
    }
    let last_ms = run.iter().map(|entry| entry.ts_ms).max().unwrap_or(0);
    let idle = cc_protocol::contracts::now_ms().saturating_sub(last_ms) / 60_000;
    drop(ok);
    drop(run);
    Some((entries, Measured { steps, edits, since_edit, checks_since, idle, edited_at, files }))
}

/// A runner job's session (docs/RUNNER.md): MANAGE's flags go to the manager as
/// `chatflow.manage.flags`, one entry per open agent, and nothing is done to the agents.
async fn publish_flags(env: &RunEnv, work: &[OpenWork]) {
    let mut agents = Vec::new();
    for item in work {
        let Some((entries, measured)) = measured(env, &item.agent, &crate::contract::commands(&item.task.contract)).await else { continue };
        let (reading, still) = flags(env, measured.since_edit, measured.idle);
        let told_since_edit = entries.iter().rev().find(|entry| entry.kind == kinds::STEER).is_some_and(|told| told.ts_ms > measured.edited_at);
        agents.push(ManageFlag { agent: item.agent.name.clone(), reading_calls: measured.since_edit as u32, idle_minutes: measured.idle, flagged: (reading || still) && !told_since_edit });
    }
    let flags = ManageFlags { session_id: env.session_id.clone(), agents };
    env.host.ctx().publish(chatflow_topics::MANAGE_FLAGS, serde_json::to_value(&flags).unwrap_or(Value::Null));
}

/// Characters of an agent's last words, and of the session's last message to it, shown per agent.
const SAID_CHARS: usize = 300;

fn short(text: &str, chars: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = text.chars().take(chars).collect();
    if cut.len() < text.len() { format!("{cut}...") } else { cut }
}

/// Entries of the project folder's top level shown to MANAGE at most.
const FOLDER_ENTRIES: usize = 60;

/// The project folder's top level as it is now, one line: each entry by name (a folder ends in
/// `/`), hidden entries left out. Read by the host at the decision, so the session compares the
/// agents' reports with what is there. Empty when the folder cannot be listed.
pub fn folder_line(env: &RunEnv) -> String {
    let Ok(listing) = std::fs::read_dir(&env.scope.workspace) else { return String::new() };
    let mut names: Vec<String> = listing
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            Some(if entry.path().is_dir() { format!("{name}/") } else { name })
        })
        .collect();
    if names.is_empty() {
        return String::new();
    }
    names.sort();
    let more = names.len().saturating_sub(FOLDER_ENTRIES);
    names.truncate(FOLDER_ENTRIES);
    let mut shown = names.join(", ");
    if more > 0 {
        shown.push_str(&env.data.text("manage_folder_more", &[("count", &more.to_string())]));
    }
    env.data.text("manage_folder", &[("entries", &shown)])
}

/// The plan the background work carries out, task by task with where each stands (done, worked on
/// by an agent, waiting for the tasks it needs), so each check compares the work with the whole
/// plan, not each agent with its own task alone. Empty without a plan being carried out.
pub async fn plan_lines(env: &RunEnv, entries: &[JournalEntry], work: &[OpenWork]) -> String {
    let Some((_, plan)) = crate::plan::latest(entries).filter(|(_, plan)| plan.status == cc_protocol::contracts::chatflow::plan_status::IMPLEMENTING) else { return String::new() };
    let mut lines = String::new();
    for task in &plan.tasks {
        let record = match &task.task_id {
            Some(id) => env.host.get::<Task>(memory_methods::TASK_GET, json!({"id": id})).await.ok(),
            None => None,
        };
        let status = record.as_ref().map(|record| record.status.as_str()).unwrap_or("pending");
        let agent = work.iter().find(|item| Some(&item.task.id) == task.task_id.as_ref()).map(|item| item.agent.name.as_str());
        let waiting: Vec<String> = task.depends_on.iter().map(u32::to_string).collect();
        let state = match (status, agent) {
            ("done", _) => env.data.text("manage_task_done", &[]),
            ("cancelled", _) => env.data.text("manage_task_cancelled", &[]),
            (_, Some(agent)) => env.data.text("manage_task_agent", &[("agent", agent)]),
            _ if !waiting.is_empty() => env.data.text("manage_task_waits", &[("tasks", &waiting.join(", "))]),
            _ => env.data.text("manage_task_pending", &[]),
        };
        lines.push_str(&env.data.text("manage_task", &[("n", &task.n.to_string()), ("title", &task.title), ("state", &state)]));
    }
    env.data.text("manage_plan", &[("goal", plan.doc.goal.trim()), ("tasks", &lines)])
}

/// One line per open agent: its task, its state, what its run did and its task's last result;
/// and whether one of them needs the session (a progress flag it was not told about since its last
/// edit; a blocked agent or a question wakes the session with its own event).
pub async fn work_lines_with_progress(env: &RunEnv, work: &[OpenWork]) -> (String, bool) {
    let mut lines = Vec::new();
    let mut needs_session = false;
    for item in work {
        let (progress, flagged) = progress(env, &item.agent, &crate::contract::commands(&item.task.contract)).await;
        needs_session |= flagged;
        lines.push(format!("{}{progress}", work_line(env, item)));
    }
    (lines.join("\n"), needs_session)
}

/// One line per open agent: its task, its state and its task's last result.
pub fn work_lines(env: &RunEnv, work: &[OpenWork]) -> String {
    work.iter().map(|item| work_line(env, item)).collect::<Vec<_>>().join("\n")
}

fn work_line(env: &RunEnv, item: &OpenWork) -> String {
    let result = item.task.result.as_deref().map(str::trim).filter(|result| !result.is_empty()).map(|result| {
        let cut: String = result.chars().take(RESULT_CHARS).collect();
        if cut.len() < result.len() { format!(": {cut}...") } else { format!(": {cut}") }
    });
    let repeated = if item.repeated { env.data.text("manage_repeat", &[]) } else { String::new() };
    env.data.text(
        "manage_line",
        &[
            ("agent", &item.agent.name),
            ("worker", worker_label(&item.agent.worker)),
            ("title", &item.task.title),
            ("state", &item.agent.status),
            ("detail", &format!("{}{repeated}", result.unwrap_or_default())),
        ],
    )
}

/// Who does an agent's work, as the lead reads it.
fn worker_label(worker: &str) -> &'static str {
    match worker {
        workers::CLAUDE => "Claude Code",
        workers::OPENAI => "Codex",
        _ => "local model",
    }
}

/// Whether a frontier CLI can take an agent's task now (`agents.workers`).
async fn frontier_ready(env: &RunEnv) -> bool {
    match env.host.get::<WorkersResult>(agents_methods::WORKERS, json!({})).await {
        Ok(listed) => listed.workers.iter().any(|info| workers::frontier(&info.id) && info.problem.is_none()),
        Err(_) => false,
    }
}

/// The session's last message to `agent` when the agent has edited nothing since (it is still
/// working on what it was told); None when it was never told or edited after.
async fn told_since_edit(env: &RunEnv, agent: &AgentRecord) -> Option<String> {
    let session_id = agent.session_id.as_deref()?;
    let journal = Journal::load(env.host.clone(), session_id).await.ok()?;
    let entries = journal.entries_vec();
    let told = entries.iter().rev().find(|entry| entry.kind == kinds::STEER)?;
    let edited = entries.iter().any(|entry| entry.seq > told.seq && entry.kind == kinds::TOOL_CALL && str_field(&entry.data, "tool") == "edit");
    (!edited).then(|| str_field(&told.data, "text").to_owned())
}

/// Cancel tasks (each with what waits for it) through the agents plugin, which stops their agents;
/// without it, only the task records. Returns the agents stopped.
pub async fn cancel_tasks(env: &RunEnv, task_ids: &[String]) -> Vec<String> {
    let mut stopped = Vec::new();
    for id in task_ids {
        match env.host.get::<CancelResult>(agents_methods::CANCEL, json!({"project_id": env.scope.project_id, "task_id": id})).await {
            Ok(result) => stopped.extend(result.agents),
            Err(error) => {
                tracing::warn!(%error, task = %id, "cancelling through the agents plugin failed; the record only");
                let _ = env.host.call(memory_methods::TASK_UPDATE, json!({"id": id, "status": "cancelled"})).await;
            }
        }
    }
    stopped
}

/// MANAGE: the model picks what happens to each open agent, the host does it and records what
/// happened (the reply reads it); a replan becomes the decision to do the work again, which the
/// DO path plans (its plan's `replaces` stops what it takes the place of).
pub async fn run(env: &RunEnv, journal: &mut Journal) -> Result<(), RpcError> {
    // A plan the agents finished is judged against what the user asked before anything else: an
    // ask not shown met sends the work on, and that is this decision ([`crate::flow::accept_tree`]).
    if crate::flow::accept_tree(env, journal).await? {
        return Ok(());
    }
    let entries = journal.entries_vec();
    let work = open_work(env, &entries).await;
    if work.is_empty() {
        let text = env.data.text("manage_none", &[]);
        return journal.note(&env.run_id, "managed", &text, true).await.map(|_| ());
    }
    // One supervisor, not two: a runner job's manager gets the flags and decides.
    if crate::runner::on(&entries) {
        publish_flags(env, &work).await;
        let text = env.data.text("managed_flags", &[]);
        return journal.note(&env.run_id, "managed", &text, false).await.map(|_| ());
    }
    // A routine check-in (not an agent's need or block) when no agent needs the session: nothing to
    // decide, so no model call (seen: 52 check-ins in four hours, most of them telling a working
    // agent again what it was told minutes before).
    let check = entries
        .iter()
        .rev()
        .find(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "run_id") == env.run_id)
        .and_then(|entry| entry.data.get("detail").and_then(|detail| detail.get("check")).and_then(Value::as_bool))
        == Some(true);
    let (lines, attention) = work_lines_with_progress(env, &work).await;
    // What the user asked of the work while it ran that no plan took up yet: the session hears it
    // once, right before its decision (seen: "the spreadsheet files must work too", promised in an
    // answer, in no task, and the work was reported done without it).
    let unplanned = match crate::plan::latest(&entries).filter(|(_, plan)| plan.status == cc_protocol::contracts::chatflow::plan_status::IMPLEMENTING) {
        Some((_, plan)) => crate::flow::asks_unplanned(&entries, &env.run_id, &plan.plan_id, true),
        None => Vec::new(),
    };
    if check && !attention && unplanned.is_empty() {
        // Transcript only: the session's conversation does not fill with check-ins that did nothing.
        let text = env.data.text("managed_nothing", &[]);
        return journal.note(&env.run_id, "managed", &text, false).await.map(|_| ());
    }
    let names: Vec<String> = work.iter().map(|item| item.agent.name.clone()).collect();
    let mut shape = crate::flow::stage_shape(env, "MANAGE")?.filled(schemas::AGENTS_PLACEHOLDER, &names, None);
    // `frontier` only while a frontier CLI can take a task now (the host knows; the model does not
    // pick what would be refused).
    if !frontier_ready(env).await {
        shape = shape.without_anywhere(workers::FRONTIER);
    }
    let asked = if unplanned.is_empty() {
        String::new()
    } else {
        let list = unplanned.iter().enumerate().map(|(index, ask)| format!("{}. {}", index + 1, env.data.text("ask_line", &[("quote", ask)]))).collect::<Vec<_>>().join("\n");
        env.data.text("manage_asks", &[("asks", &list)])
    };
    // The plan, then the folder as it is now, then each agent's report; what the user asked last.
    let live = format!("{}{}{}{asked}", plan_lines(env, &entries, &work).await, folder_line(env), env.data.text("manage_live", &[("work", &lines)]));
    let (decision, _) = crate::flow::decide_json::<Manage>(env, journal, "MANAGE", &shape, &live).await?;
    let mut done = Vec::new();
    for action in &decision.actions {
        let Some(item) = work.iter().find(|item| item.agent.name == action.agent) else { continue };
        let name = item.agent.name.as_str();
        let text = action.text.trim();
        let params = json!({"project_id": env.scope.project_id, "agent": name});
        let outcome = match action.action.as_str() {
            "stop" => match env.host.get::<CancelResult>(agents_methods::CANCEL, json!({"project_id": env.scope.project_id, "task_id": item.task.id})).await {
                Ok(result) => Ok(env.data.text("managed_stop", &[("agent", name), ("count", &result.tasks.len().to_string())])),
                Err(error) => Err(error),
            },
            // The session takes the task as done: the agent stops now and the steward judges its
            // work (a message saying "stop" left the agent's own checks to put it back to work).
            "finish" => match env.host.call(agents_methods::FINISH, params.clone()).await {
                Ok(result) if result.get("verified").and_then(Value::as_bool) == Some(true) => Ok(env.data.text("managed_finish", &[("agent", name)])),
                Ok(_) => Ok(env.data.text("managed_finish_failing", &[("agent", name)])),
                Err(error) => Err(error),
            },
            "restart" if item.repeated => Ok(env.data.text("managed_restart_refused", &[("agent", name)])),
            "restart" if !matches!(item.agent.status.as_str(), "blocked" | "stopped") => {
                // A working agent is not restarted; what to do differently reaches it as a message.
                let steered = if text.is_empty() { Ok(Value::Null) } else { env.host.call(agents_methods::STEER, json!({"project_id": env.scope.project_id, "agent": name, "text": text, "kind": "steer"})).await };
                steered.map(|_| env.data.text("managed_working", &[("agent", name)]))
            }
            "restart" => {
                let mut outcome = Ok(Value::Null);
                if !text.is_empty() {
                    outcome = env.host.call(agents_methods::STEER, json!({"project_id": env.scope.project_id, "agent": name, "text": text, "kind": "followup"})).await;
                }
                match outcome {
                    Ok(_) => env.host.call(agents_methods::RESTART, params).await.map(|_| env.data.text("managed_restart", &[("agent", name)])),
                    Err(error) => Err(error),
                }
            }
            // The agent's task goes to the other worker: a new agent takes it (agents.agent.worker);
            // `text` reaches the new agent as its first steer.
            "local" | "frontier" => match env.host.get::<AgentRecord>(agents_methods::AGENT_WORKER, json!({"project_id": env.scope.project_id, "agent": name, "worker": action.action})).await {
                Ok(moved) if moved.name == name => Ok(env.data.text("managed_same_worker", &[("agent", name), ("worker", worker_label(&moved.worker))])),
                Ok(moved) => {
                    if !text.is_empty() {
                        let _ = env.host.call(agents_methods::STEER, json!({"project_id": env.scope.project_id, "agent": moved.name, "text": text, "kind": "steer"})).await;
                    }
                    Ok(env.data.text("managed_moved", &[("agent", name), ("new", &moved.name), ("worker", worker_label(&moved.worker))]))
                }
                Err(error) => Err(error),
            },
            // One message per agent until it edits: a check-in sends nothing to an agent still working
            // on what it was told (seen: "READING WITHOUT WRITING" sent eight times to one agent, once
            // at 30 edits and one call since its last edit); a need or a block may answer at once,
            // but not with the same words again.
            "message" if !text.is_empty()
                && told_since_edit(env, &item.agent).await.is_some_and(|told| check || told.split_whitespace().eq(text.split_whitespace())) =>
            {
                Ok(env.data.text("managed_repeat", &[("agent", name)]))
            }
            "message" if !text.is_empty() => {
                let kind = if item.agent.status == "working" { "steer" } else { "followup" };
                env.host
                    .call(agents_methods::STEER, json!({"project_id": env.scope.project_id, "agent": name, "text": text, "kind": kind}))
                    .await
                    .map(|_| env.data.text("managed_message", &[("agent", name), ("text", text)]))
            }
            _ => continue,
        };
        done.push(outcome.unwrap_or_else(|error| env.data.text("managed_failed", &[("action", &action.action), ("agent", name), ("error", &error.message)])));
    }
    let replan = decision.replan.trim() == "yes" && !decision.brief.trim().is_empty();
    if replan {
        done.push(env.data.text("managed_replan", &[("brief", decision.brief.trim())]));
    }
    if done.is_empty() {
        done.push(env.data.text("managed_nothing", &[]));
    }
    let managed = journal.note(&env.run_id, "managed", &done.join("\n"), true).await?;
    if !unplanned.is_empty() {
        // Heard: these asks are not shown to the next check again.
        journal.set(managed, "asks_told", json!(true)).await?;
    }
    if replan {
        let detail = json!({
            "missing": "",
            "route": "DO",
            "effort": schemas::effort_or_default(""),
            "symbols": [],
            "files": [],
            "queries": [],
            "brief": decision.brief.trim(),
            "checks": [],
            "skills": [],
            "note": "",
            "by": "host",
        });
        journal.append(kinds::ROUTE, json!({"route": "DO", "stage": "decide", "detail": detail, "run_id": env.run_id})).await?;
    }
    Ok(())
}
