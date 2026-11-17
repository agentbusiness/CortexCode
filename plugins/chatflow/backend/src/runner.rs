//! Runner mode (`runner@1`, docs/RUNNER.md): a session that builds for a manager. The runner plugin
//! turns it on (`chatflow.runner.set`); everything about it is in the session's journal, so a
//! hot-swapped successor reads the same state.
//!
//! In runner mode:
//! 1. the session's plan is carried out here, one task after the other (one working tree, the
//!    runner commits after each task), also when it has several tasks;
//! 2. a message during a task is that task's next step, not a request for a new plan (the
//!    manager's steers are fixed sentences about the task in work);
//! 3. a replan is held: recorded as a `note` (purpose `replan_held`), published as
//!    `chatflow.replan.held`, and applied only when `chatflow.replan.answer` accepts it;
//! 4. MANAGE publishes its flags (`chatflow.manage.flags`) and does nothing else.

use crate::agent_loop::RunEnv;
use crate::journal::Journal;
use crate::plan::Plan;
use cc_protocol::RpcError;
use cc_protocol::contracts::chatflow::{PlanPut, ReplanHeld, plan_status, topics};
use cc_protocol::contracts::frame::{JournalEntry, SessionStatus, kinds};
use serde_json::{Value, json};
use std::time::Duration;

/// `purpose` of the note that turns the mode on or off (`{on}`).
pub const MODE: &str = "runner_mode";
/// `purpose` of the note that holds a replan ([`ReplanHeld`] plus `state`).
pub const HELD: &str = "replan_held";

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

fn note<'a>(entries: &'a [JournalEntry], purpose: &str) -> Option<&'a JournalEntry> {
    entries.iter().rev().find(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == purpose)
}

/// Whether the session is in runner mode: what its newest `runner_mode` note says.
pub fn on(entries: &[JournalEntry]) -> bool {
    note(entries, MODE).and_then(|entry| entry.data.get("on")).and_then(Value::as_bool).unwrap_or(false)
}

/// The session's held replan that nobody answered yet.
pub fn pending(entries: &[JournalEntry]) -> Option<&JournalEntry> {
    note(entries, HELD).filter(|entry| str_field(&entry.data, "state") == "pending")
}

/// The plan `chatflow.plan.put` records: the tasks numbered in their order, each dependency kept
/// when it names an earlier task, approved. A task without a title is refused (the numbers are
/// the manager's: none may shift).
pub fn plan_of(put: &PlanPut) -> Result<Plan, String> {
    if put.tasks.is_empty() {
        return Err("a plan needs at least one task".into());
    }
    let clean = |items: &[String]| -> Vec<String> { items.iter().map(|item| item.trim().to_owned()).filter(|item| !item.is_empty()).collect() };
    let mut tasks = Vec::with_capacity(put.tasks.len());
    for (index, task) in put.tasks.iter().enumerate() {
        let n = index as u32 + 1;
        let title = task.title.trim();
        if title.is_empty() {
            return Err(format!("task {n} has no title"));
        }
        let mut depends_on: Vec<u32> = task.depends_on.iter().copied().filter(|d| *d >= 1 && *d < n).collect();
        depends_on.sort_unstable();
        depends_on.dedup();
        tasks.push(crate::plan::PlanTask {
            n,
            title: title.to_owned(),
            brief: task.brief.trim().to_owned(),
            known: clean(&task.known),
            rules: clean(&task.rules),
            depends_on,
            touches: clean(&task.touches),
            checks: clean(&task.checks),
            covers: Vec::new(),
            skills: Vec::new(),
            task_id: None,
        });
    }
    Ok(Plan {
        plan_id: crate::plan::new_id(),
        version: 1,
        doc: crate::plan::PlanDoc {
            analysis: String::new(),
            goal: put.goal.trim().to_owned(),
            approach: put.approach.trim().to_owned(),
            steps: tasks.iter().map(|task| task.title.clone()).collect(),
            architecture: String::new(),
            risks: Vec::new(),
        },
        tasks,
        status: plan_status::APPROVED.into(),
        ..Plan::default()
    })
}

/// Hold the replan `made` (the next version of the run's plan, not yet written): record it,
/// publish it, and wait for the answer. True when it was accepted. A run that stops while it
/// waits leaves the replan `dropped`, so nothing stays pending.
pub async fn hold(env: &RunEnv, journal: &mut Journal, made: &Plan, after_task: u32, outcome: &str, found: &str) -> Result<bool, RpcError> {
    // A replan a stopped predecessor left unanswered is over: only this one waits.
    let stale: Vec<u64> = journal.entries().filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == HELD && str_field(&entry.data, "state") == "pending").map(|entry| entry.seq).collect();
    for seq in stale {
        journal.set(seq, "state", json!("dropped")).await?;
    }
    let kept = made.steps_done.len();
    let tasks: Vec<Value> = made.tasks.iter().skip(kept).filter_map(|task| serde_json::to_value(task).ok()).collect();
    let titles: String = made.tasks.iter().skip(kept).map(|task| env.data.text("list_item", &[("text", &task.title)])).collect::<Vec<_>>().join("\n");
    let text = env.data.text("replan_held", &[("n", &after_task.to_string()), ("found", found.trim()), ("tasks", &titles)]);
    let mut held = ReplanHeld { session_id: env.session_id.clone(), seq: 0, found: found.trim().to_owned(), outcome: outcome.to_owned(), after_task, tasks };
    let mut data = serde_json::to_value(&held)?;
    if let Some(map) = data.as_object_mut() {
        map.insert("text".into(), json!(text));
        map.insert("purpose".into(), json!(HELD));
        map.insert("run_id".into(), json!(env.run_id));
        map.insert("host_message".into(), json!(false));
        map.insert("state".into(), json!("pending"));
    }
    let seq = journal.append(kinds::NOTE, data).await?;
    held.seq = seq;
    env.host.ctx().publish(topics::REPLAN_HELD, serde_json::to_value(&held)?);
    let _ = env.host.session_status(&env.session_id, SessionStatus::Waiting).await;
    env.set_stage("replan");
    let state = loop {
        let fresh = env.host.journal_read(&env.session_id, seq).await?;
        let Some(entry) = fresh.into_iter().find(|entry| entry.seq == seq) else { return Err(RpcError::internal("the held replan vanished")) };
        let state = str_field(&entry.data, "state").to_owned();
        if state != "pending" {
            break state;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            _ = env.stops.cancel.cancelled() => break "dropped".to_owned(),
            _ = env.stops.park.cancelled() => break "dropped".to_owned(),
        }
    };
    journal.set(seq, "state", json!(state)).await?;
    if state == "dropped" {
        return Err(RpcError::cancelled());
    }
    let _ = env.host.session_status(&env.session_id, SessionStatus::Running).await;
    env.set_stage("plan");
    Ok(state == "accepted")
}
