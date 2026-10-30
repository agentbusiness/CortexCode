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
use cc_protocol::contracts::agents::{CancelResult, ListResult, events, methods as agents_methods};
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
/// edits and the files they changed, calls since its last edit, and how long ago it last moved.
/// The host names what needs the session's attention: calls without an edit past
/// `limits.manage_reading_calls`, or no step for `limits.manage_idle_minutes` (waiting for the model).
pub async fn progress(env: &RunEnv, agent: &AgentRecord) -> String {
    let Some(session_id) = agent.session_id.as_deref() else { return String::new() };
    let Ok(journal) = Journal::load(env.host.clone(), session_id).await else { return String::new() };
    let entries = journal.entries_vec();
    let Some(run_id) = entries.iter().rev().find(|entry| entry.kind == kinds::STATUS).map(|entry| str_field(&entry.data, "run_id").to_owned()) else {
        return String::new();
    };
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
    let told = entries.iter().rev().find(|entry| entry.kind == kinds::STEER);
    let told_at = told.map(|entry| entry.ts_ms).unwrap_or(0);
    let mut edits = 0;
    let mut since_edit = 0;
    let mut files: Vec<String> = Vec::new();
    for entry in run.iter().filter(|entry| entry.kind == kinds::TOOL_CALL && entry.data.get("host").and_then(Value::as_bool) != Some(true)) {
        let edited = str_field(&entry.data, "tool") == "edit" && ok.get(str_field(&entry.data, "call_id")).copied().unwrap_or(false);
        if edited {
            edits += 1;
            since_edit = 0;
            let path = entry.data.get("args").and_then(|args| args.get("path")).and_then(Value::as_str).unwrap_or("");
            if !path.is_empty() && !files.iter().any(|known| known == path) {
                files.push(path.to_owned());
            }
        // Calls that did not run (skipped for a message, an edit refused by another agent's lock)
        // are not reading.
        } else if entry.ts_ms >= told_at && ok.get(str_field(&entry.data, "call_id")).copied().unwrap_or(false) {
            since_edit += 1;
        }
    }
    let last_ms = run.iter().map(|entry| entry.ts_ms).max().unwrap_or(0);
    let idle = cc_protocol::contracts::now_ms().saturating_sub(last_ms) / 60_000;
    let files = if files.is_empty() { env.data.text("manage_no_files", &[]) } else { files.join(", ") };
    let mut line = env.data.text(
        "manage_progress",
        &[("steps", &steps.to_string()), ("edits", &edits.to_string()), ("files", &files), ("since", &since_edit.to_string()), ("idle", &idle.to_string())],
    );
    let limits = &env.data.limits;
    if limits.manage_reading_calls > 0 && since_edit >= limits.manage_reading_calls as usize {
        line.push_str(&env.data.text("manage_flag_reading", &[("since", &since_edit.to_string())]));
    }
    if limits.manage_idle_minutes > 0 && idle >= limits.manage_idle_minutes {
        line.push_str(&env.data.text("manage_flag_idle", &[("idle", &idle.to_string())]));
    }
    // Both ways: what the session last told it, and what it last said (its answer, its question).
    if let Some(told) = told {
        let ago = cc_protocol::contracts::now_ms().saturating_sub(told.ts_ms) / 60_000;
        line.push_str(&env.data.text("manage_told", &[("ago", &ago.to_string()), ("text", &short(str_field(&told.data, "text"), SAID_CHARS))]));
    }
    if let Some(said) = entries.iter().rev().find(|entry| entry.kind == kinds::ASSISTANT_MESSAGE && !str_field(&entry.data, "text").trim().is_empty()) {
        line.push_str(&env.data.text("manage_said", &[("text", &short(str_field(&said.data, "text"), SAID_CHARS))]));
    }
    line
}

/// Characters of an agent's last words, and of the session's last message to it, shown per agent.
const SAID_CHARS: usize = 300;

fn short(text: &str, chars: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = text.chars().take(chars).collect();
    if cut.len() < text.len() { format!("{cut}...") } else { cut }
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

/// One line per open agent: its task, its state, what its run did and its task's last result.
pub async fn work_lines_with_progress(env: &RunEnv, work: &[OpenWork]) -> String {
    let mut lines = Vec::new();
    for item in work {
        lines.push(format!("{}{}", work_line(env, item), progress(env, &item.agent).await));
    }
    lines.join("\n")
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
        &[("agent", &item.agent.name), ("title", &item.task.title), ("state", &item.agent.status), ("detail", &format!("{}{repeated}", result.unwrap_or_default()))],
    )
}

/// Whether the session's last message to `agent` says `text` (spacing aside) and the agent edited
/// nothing since.
async fn told_already(env: &RunEnv, agent: &AgentRecord, text: &str) -> bool {
    let Some(session_id) = agent.session_id.as_deref() else { return false };
    let Ok(journal) = Journal::load(env.host.clone(), session_id).await else { return false };
    let entries = journal.entries_vec();
    let Some(told) = entries.iter().rev().find(|entry| entry.kind == kinds::STEER) else { return false };
    let same = |a: &str, b: &str| a.split_whitespace().eq(b.split_whitespace());
    same(str_field(&told.data, "text"), text)
        && !entries.iter().any(|entry| entry.seq > told.seq && entry.kind == kinds::TOOL_CALL && str_field(&entry.data, "tool") == "edit")
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
    let entries = journal.entries_vec();
    let work = open_work(env, &entries).await;
    if work.is_empty() {
        let text = env.data.text("manage_none", &[]);
        return journal.note(&env.run_id, "managed", &text, true).await.map(|_| ());
    }
    let names: Vec<String> = work.iter().map(|item| item.agent.name.clone()).collect();
    let shape = crate::flow::stage_shape(env, "MANAGE")?.filled(schemas::AGENTS_PLACEHOLDER, &names, None);
    let live = format!("{}{}", plan_lines(env, &entries, &work).await, env.data.text("manage_live", &[("work", &work_lines_with_progress(env, &work).await)]));
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
            // The same words again, before the agent edited anything since: it has them already.
            "message" if !text.is_empty() && told_already(env, &item.agent, text).await => Ok(env.data.text("managed_repeat", &[("agent", name)])),
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
    journal.note(&env.run_id, "managed", &done.join("\n"), true).await?;
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
