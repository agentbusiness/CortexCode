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

/// One line per open agent: its task, its state and its task's last result.
pub fn work_lines(env: &RunEnv, work: &[OpenWork]) -> String {
    work.iter()
        .map(|item| {
            let result = item.task.result.as_deref().map(str::trim).filter(|result| !result.is_empty()).map(|result| {
                let cut: String = result.chars().take(RESULT_CHARS).collect();
                if cut.len() < result.len() { format!(": {cut}...") } else { format!(": {cut}") }
            });
            let repeated = if item.repeated { env.data.text("manage_repeat", &[]) } else { String::new() };
            env.data.text(
                "manage_line",
                &[("agent", &item.agent.name), ("title", &item.task.title), ("state", &item.agent.status), ("detail", &format!("{}{repeated}", result.unwrap_or_default()))],
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    let live = env.data.text("manage_live", &[("work", &work_lines(env, &work))]);
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
            "message" if !text.is_empty() => {
                let kind = if item.agent.status == "working" { "steer" } else { "followup" };
                env.host
                    .call(agents_methods::STEER, json!({"project_id": env.scope.project_id, "agent": name, "text": text, "kind": kind}))
                    .await
                    .map(|_| env.data.text("managed_message", &[("agent", name)]))
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
