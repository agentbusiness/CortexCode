//! The foreground turn, a router-first flow recast for coding:
//! 1. ROUTE: one constrained generation writes `work`, then the route (ANSWER, LOOKUP, DO,
//!    DELEGATE, STATUS, PLAN, IMPLEMENT), then, on the routes that work, the skills whose summary
//!    names that work ([`crate::skillsel`]), and what to read. The chosen skills are injected once,
//!    before anything else of the turn runs.
//! 2. Host reads run concurrently (graph and workspace) and are journaled.
//! 3. DECIDE writes `missing` first, then MORE (another round of reads), ANSWER, DO or DELEGATE.
//! 4. The result: a streamed reply, the coding agent loop, or a delegation to background agents.
//!
//! Around them, the plan: the conversation is a plain chat; DECIDE (or ROUTE) PLAN runs PLAN, or
//! PLAN_EDIT while a plan is open: the plan's points, nothing changed ([`crate::plan`]); ROUTE
//! IMPLEMENT (or the plan card's Implement) hands the open plan to the foreground loop (one task)
//! or to memory tasks and agents (several).
//!
//! The turn is a state machine over the journal: each stage is derived from the run's entries, so a
//! resumed run (after a hot-swap or crash) continues at the stage it reached.

use crate::agent_loop::{self, LoopKind, LoopOutcome, LoopSpec, RunEnv};
use crate::contract;
use crate::journal::Journal;
use crate::llm;
use crate::prompts;
use crate::reads::{self, ReadReq, Wanted};
use crate::skillsel;
use crate::schemas::{self, Decision, KnowledgeProposed, PlanOutput, RouterDecision, Shape, parse_json};
use crate::{plan, proposal};
use cc_protocol::contracts::chatflow::{kinds as chatflow_kinds, plan_status};
use cc_protocol::contracts::agents::{DelegateResult, methods as agents_methods};
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::lang::LanguageInfo;
use cc_protocol::contracts::memory::{KnowledgeItem, KnowledgeListResult, KnowledgeSearchResult, Task, methods as memory_methods};
use cc_protocol::contracts::tools::methods as tools_methods;
use cc_protocol::contracts::model::ChatMessage;
use cc_protocol::contracts::now_ms;
use cc_protocol::{RpcError, codes};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};


/// How a turn ended.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnd {
    /// done | failed | cancelled | blocked | stopped | parked
    pub state: String,
    pub detail: String,
    pub route: String,
    pub agent: Option<LoopOutcome>,
}

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

#[derive(Debug, Clone, PartialEq)]
enum Stage {
    Route,
    /// ROUTE's chosen skills go into the conversation (a host note), before the rest of the turn.
    Skills { route_seq: u64 },
    Reads { from_seq: u64 },
    StatusReads,
    /// MANAGE: the session's own agents are stopped, restarted or messaged, and the work may be
    /// planned again ([`crate::manage`]).
    Manage,
    Decide { allow_more: bool },
    /// A change request: the host records the decision to do it from ROUTE's own fields (no model
    /// call, no reads), so the plan comes next; discovery happens in the tasks.
    Commit { route_seq: u64 },
    Reply,
    /// The DO loop; `step` (1-based) when the decision's plan is carried out one step at a time.
    Act { decision_seq: u64, step: Option<u32> },
    /// A LOOKUP that named files: the host reads their diagnostics before the question loop.
    Diagnostics { route_seq: u64 },
    /// A question: the model explores with its tools and answers (no fixed reads).
    Explore,
    Delegate { decision_seq: u64 },
    /// PLAN, or PLAN_EDIT while a plan is open, after a decision to plan. `auto`: the plan of a DO
    /// decision, written before the work and carried out at once by the Act stage, step by step.
    Plan { decision_seq: u64, auto: bool },
    /// IMPLEMENT: the open plan goes to the task machinery.
    Implement { route_seq: u64 },
    /// After the answer, when the router judged that the message states
    /// something durable about the project's world (docs/PROJECT-KNOWLEDGE-PLAN.md P3).
    Knowledge { route_seq: u64 },
    /// The message sets aside the direction the user gave earlier: the conversation's decisions
    /// in the project's knowledge are withdrawn, before anything of the turn reads them.
    SetAside { route_seq: u64 },
    Done,
}

fn of_run<'a>(entries: &'a [JournalEntry], run_id: &'a str) -> Vec<&'a JournalEntry> {
    entries.iter().filter(|entry| str_field(&entry.data, "run_id") == run_id).collect()
}

fn host_reads_after(run: &[&JournalEntry], seq: u64) -> usize {
    run.iter().filter(|entry| entry.seq > seq && entry.kind == kinds::TOOL_CALL && entry.data.get("host").and_then(Value::as_bool) == Some(true)).count()
}

fn note_after(run: &[&JournalEntry], seq: u64, purpose: &str) -> bool {
    run.iter().any(|entry| entry.seq > seq && entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == purpose)
}

/// A decision's list field (`symbols`, `files`, `queries`, `focus`).
fn detail_list(decision: &JournalEntry, field: &str) -> Vec<String> {
    decision
        .data
        .get("detail")
        .and_then(|detail| detail.get(field))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// The files the run's decisions named (ROUTE's and each DECIDE's `files`), once each, in order:
/// what the project's check commands that take files are filled with.
pub(crate) fn run_files(entries: &[JournalEntry], run_id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in entries.iter().filter(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "run_id") == run_id) {
        for file in detail_list(entry, "files") {
            let file = file.trim().to_owned();
            if !file.is_empty() && !out.contains(&file) {
                out.push(file);
            }
        }
    }
    out
}

/// Whether a read round reads diagnostics: ROUTE's round of a change (DO, DELEGATE, PLAN).
fn reads_diagnostics(decision: &JournalEntry, route: &str) -> bool {
    str_field(&decision.data, "stage") == "route" && matches!(route, "DO" | "DELEGATE" | "PLAN")
}

/// The host reads of a read round: what the decision (ROUTE, or a DECIDE asking for MORE) named.
/// ROUTE's round of a change also reads, for the session, the diagnostics of the named files a
/// language server checks (`languages`), or the workspace's when it names none: a code's skill
/// comes with them, once per session.
fn round_reads(scope: &reads::Scope, session_id: &str, decision: &JournalEntry, route: &str, limits: &crate::data::Limits, languages: &[LanguageInfo]) -> Vec<ReadReq> {
    let list = |field: &str| detail_list(decision, field);
    let (symbols, files, queries, focus) = (list("symbols"), list("files"), list("queries"), list("focus"));
    let first = str_field(&decision.data, "stage") == "route";
    // The searches the decision named are the topic; the work sentence restates the request
    // ("The user wants ...") and finds code that matches the request's words, not its subject.
    let work = (first && queries.iter().all(|query| query.trim().is_empty())).then(||decision.data.get("detail").map(|detail| str_field(detail, "work").to_owned()).unwrap_or_default());
    let change = matches!(route, "DO" | "DELEGATE" | "PLAN");
    let diagnostics = reads_diagnostics(decision, route).then_some(reads::DiagnosticsFor { session_id, languages, shown: limits.diagnostics_shown });
    let wanted = Wanted { symbols: &symbols, files: &files, queries: &queries, focus: &focus, work: work.as_deref(), change, session_id, diagnostics };
    reads::plan(scope, &wanted, limits)
}

/// A LOOKUP's host reads before its question loop: the diagnostics of the files ROUTE named that a
/// language server checks.
fn lookup_reads(scope: &reads::Scope, session_id: &str, route: &JournalEntry, languages: &[LanguageInfo]) -> Vec<ReadReq> {
    reads::diagnostics(scope, session_id, &[&detail_list(route, "files"), &detail_list(route, "focus")], languages)
}

/// STATUS's host reads: the project's agents, scheduler, tasks and board.
fn status_reads(scope: &reads::Scope) -> Vec<ReadReq> {
    let project = json!({"project_id": scope.project_id});
    vec![
        ReadReq { tool: agents_methods::LIST.into(), args: project.clone(), intent: "Background agents".into(), impact: false },
        ReadReq { tool: agents_methods::SCHEDULER.into(), args: project.clone(), intent: "Scheduler".into(), impact: false },
        ReadReq { tool: memory_methods::TASK_LIST.into(), args: project.clone(), intent: "Tasks".into(), impact: false },
        ReadReq { tool: memory_methods::BOARD_LIST.into(), args: project, intent: "Project board".into(), impact: false },
    ]
}

/// Whether ROUTE named any file or focus item (non-blank).
fn names_files(route: &JournalEntry) -> bool {
    ["files", "focus"].iter().any(|field| detail_list(route, field).iter().any(|item| !item.trim().is_empty()))
}

fn reply_finished(run: &[&JournalEntry], seq: u64) -> bool {
    run.iter().any(|entry| {
        entry.seq > seq
            && entry.kind == kinds::ASSISTANT_MESSAGE
            && str_field(&entry.data, "stage") == "REPLY"
            && entry.data.get("streaming").and_then(Value::as_bool) == Some(false)
            && entry.data.get("interrupted").and_then(Value::as_bool) != Some(true)
    })
}

fn plan_written(run: &[&JournalEntry], seq: u64) -> bool {
    run.iter().any(|entry| entry.seq > seq && entry.kind == chatflow_kinds::PLAN)
}

/// The stage a run is at, derived from its journal entries.
/// `read_rounds` (data: `limits.read_rounds`) ends a read loop, never output:
/// after the last round DECIDE can only answer or act.
fn derive(entries: &[JournalEntry], run_id: &str, read_rounds_limit: usize) -> Stage {
    let run = of_run(entries, run_id);
    let Some(route) = run.iter().find(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "stage") == "route") else {
        return derive_work(entries, run_id, read_rounds_limit);
    };
    // The skills ROUTE chose: everything after the route sees them.
    if !skillsel::of_decision(route).is_empty() && !skillsel::injected(run.iter().copied(), route.seq) {
        return Stage::Skills { route_seq: route.seq };
    }
    if route.data.get("drops").and_then(Value::as_bool) == Some(true) && !note_after(&run, route.seq, "set_aside") {
        return Stage::SetAside { route_seq: route.seq };
    }
    let stage = derive_work(entries, run_id, read_rounds_limit);
    if stage == Stage::Done && route.data.get("teaches").and_then(Value::as_bool) == Some(true) && !note_after(&run, route.seq, "knowledge_done") {
        return Stage::Knowledge { route_seq: route.seq };
    }
    stage
}

/// The stage of the run's work (routes, reads, decisions and their outcome).
fn derive_work(entries: &[JournalEntry], run_id: &str, read_rounds_limit: usize) -> Stage {
    let run = of_run(entries, run_id);
    let routes: Vec<&&JournalEntry> = run.iter().filter(|entry| entry.kind == kinds::ROUTE).collect();
    let Some(last) = routes.last() else { return Stage::Route };
    let route = str_field(&last.data, "route");
    let decides = routes.iter().filter(|entry| str_field(&entry.data, "stage") == "decide").count();
    // A round is a decision followed by at least one host read before the next decision.
    let read_rounds = routes
        .iter()
        .enumerate()
        .filter(|(index, entry)| {
            let until = routes.get(index + 1).map(|next| next.seq).unwrap_or(u64::MAX);
            run.iter().any(|read| {
                read.seq > entry.seq
                    && read.seq < until
                    && read.kind == kinds::TOOL_CALL
                    && read.data.get("host").and_then(Value::as_bool) == Some(true)
            })
        })
        .count();
    if str_field(&last.data, "stage") == "route" {
        return match route {
            // The work went to agents, one per unit of a skill (`units_delegation`).
            "LOOKUP" | "DO" if note_after(&run, last.seq, "implement_done") => Stage::Done,
            "LOOKUP" | "ANSWER" if note_after(&run, last.seq, "explore_done") => Stage::Done,
            "LOOKUP" if names_files(last) && host_reads_after(&run, last.seq) == 0 && !note_after(&run, last.seq, "diagnostics_read") => {
                Stage::Diagnostics { route_seq: last.seq }
            }
            "LOOKUP" | "ANSWER" => Stage::Explore,
            "MANAGE" if reply_finished(&run, last.seq) => Stage::Done,
            // A regular check ends with what it did (the `managed` note the person sees): no reply.
            "MANAGE" if note_after(&run, last.seq, "managed") && last.data.get("detail").and_then(|detail| detail.get("check")).and_then(Value::as_bool) == Some(true) => Stage::Done,
            "MANAGE" if note_after(&run, last.seq, "managed") => Stage::Reply,
            "MANAGE" => Stage::Manage,
            "STATUS" if reply_finished(&run, last.seq) || note_after(&run, last.seq, "explore_done") => Stage::Done,
            // After the board, tasks and scheduler, the question loop: whether a piece of the work
            // is there, or works, is checked in the files, not taken from the agents' reports
            // (seen: "is the new harness ready?" answered from an old handoff, unchecked).
            "STATUS" if host_reads_after(&run, last.seq) > 0 => Stage::Explore,
            "STATUS" => Stage::StatusReads,
            "IMPLEMENT" if note_after(&run, last.seq, "implement_done") => Stage::Done,
            "IMPLEMENT" => Stage::Implement { route_seq: last.seq },
            // A plan card's Revise names its plan: the comments are the change, PLAN_EDIT writes it.
            "PLAN" if !str_field(&last.data, "plan_id").is_empty() && plan_written(&run, last.seq) => Stage::Done,
            "PLAN" if !str_field(&last.data, "plan_id").is_empty() => Stage::Plan { decision_seq: last.seq, auto: false },
            // A change: plan first, from the conversation and the repository map in the prefix (no
            // discovery before it); each task then reads what it needs.
            "DO" => Stage::Commit { route_seq: last.seq },
            // No generic reads after ROUTE (a plan, a delegation): DECIDE decides from the
            // conversation and the brief, and reads only what it names (MORE).
            _ => Stage::Decide { allow_more: read_rounds < read_rounds_limit },
        };
    }
    match route {
        "MORE" => {
            if host_reads_after(&run, last.seq) > 0 {
                Stage::Decide { allow_more: read_rounds < read_rounds_limit && decides < read_rounds_limit * 2 }
            } else if note_after(&run, last.seq, "no_reads") {
                // The requested reads were all made before: decide without another read round.
                Stage::Decide { allow_more: false }
            } else {
                Stage::Reads { from_seq: last.seq }
            }
        }
        "ANSWER" if reply_finished(&run, last.seq) => Stage::Done,
        "ANSWER" => Stage::Reply,
        "DO" if note_after(&run, last.seq, "act_done") && reply_finished(&run, last.seq) => Stage::Done,
        // The last task's reply is a report for the host (what changed, each test); the user gets
        // their own answer after the work, written from their asks.
        "DO" if note_after(&run, last.seq, "act_done") => Stage::Reply,
        // The auto plan's tasks went to agents at once (computers were free for them).
        "DO" if note_after(&run, last.seq, "implement_done") => Stage::Done,
        // A decided change is planned first, then carried out one step at a time (an IMPLEMENT
        // decision comes from a plan already and runs as one loop).
        "DO" if str_field(&last.data, "stage") == "decide" => match plan::for_run(entries, run_id).filter(|(seq, _)| *seq > last.seq) {
            None => Stage::Plan { decision_seq: last.seq, auto: true },
            Some((_, plan)) => {
                let total = plan.tasks.len() as u32;
                let done = plan.steps_done.len() as u32;
                Stage::Act { decision_seq: last.seq, step: (total > 0).then(|| (done + 1).min(total)) }
            }
        },
        "DO" => Stage::Act { decision_seq: last.seq, step: None },
        "DELEGATE" if reply_finished(&run, last.seq) => Stage::Done,
        "DELEGATE" if note_after(&run, last.seq, "delegated") => Stage::Reply,
        "DELEGATE" => Stage::Delegate { decision_seq: last.seq },
        "PLAN" if plan_written(&run, last.seq) => Stage::Done,
        "PLAN" => Stage::Plan { decision_seq: last.seq, auto: false },
        _ => Stage::Reply,
    }
}

/// A constrained stage's shape from the data set.
pub(crate) fn stage_shape(env: &RunEnv, stage: &str) -> Result<schemas::Shape, RpcError> {
    env.data.shape(stage).cloned().ok_or_else(|| RpcError::internal(format!("the data set has no schema for {stage}")))
}

/// The skills catalog as a choosing stage's message shows it (volatile text, after the cached
/// prefix): the catalog lines in `catalog`'s order (the previous turn's skills first), then the
/// note naming those previous skills. Empty when the stage offers no choice.
/// When the catalog is in the session's system message ([`crate::context::Prefix::catalog_in_prefix`])
/// only the note is shown here.
fn skills_live(data: &crate::data::Dataset, prefix: &crate::context::Prefix, catalog: &[String], previous: &[String]) -> String {
    if catalog.is_empty() || data.limits.skills_per_turn == 0 {
        return String::new();
    }
    let mut parts = Vec::new();
    if !prefix.catalog_in_prefix(data) {
        let lines: Vec<String> = catalog.iter().map(|uri| prefix.skill_line(uri)).collect();
        parts.push(prompts::skills_list(data, &lines));
    }
    let shown: Vec<&str> = previous.iter().filter(|uri| catalog.contains(uri)).map(String::as_str).collect();
    if !shown.is_empty() {
        parts.push(data.text("skills_previous", &[("uris", &shown.join(", "))]));
    }
    parts.join("\n")
}

/// ROUTE's shape with its `skills` pick filled from the session's catalog (the previous turn's
/// skills first), and the stage-message text showing that catalog.
fn route_shape(env: &RunEnv, entries: &[JournalEntry]) -> Result<(schemas::Shape, Vec<String>, String), RpcError> {
    let previous = skillsel::previous_turn(entries, &env.run_id);
    let catalog = skillsel::ordered_catalog(&env.prefix.skills, &previous);
    let shape = stage_shape(env, "ROUTE")?.with_catalog(&catalog, env.data.limits.skills_per_turn);
    let live = skills_live(&env.data, &env.prefix, &catalog, &previous);
    Ok((shape, catalog, live))
}

fn base_messages(env: &RunEnv, entries: &[JournalEntry]) -> Vec<ChatMessage> {
    crate::prewarm::conversation(&env.prefix, &env.data, entries, env.image_input)
}

fn next_step(journal: &Journal, run_id: &str) -> u64 {
    agent_loop::next_step(&journal.entries_vec(), run_id)
}

/// One constrained decision (router, decide, checks), retried once with the parse error.
pub(crate) async fn decide_json<T: serde::de::DeserializeOwned>(
    env: &RunEnv,
    journal: &mut Journal,
    stage: &str,
    shape: &schemas::Shape,
    live: &str,
) -> Result<(T, String), RpcError> {
    let grammar = schemas::grammar(shape);
    let any = env.stops.any();
    let mut live = live.to_owned();
    let mut last_error = String::new();
    for _attempt in 0..2 {
        let messages = crate::fit::fitted(env, journal, |entries| {
            let mut messages = base_messages(env, entries);
            messages.push(prompts::stage_message(&env.data, &env.prefix.role, stage, &live));
            messages
        })
        .await?;
        let mut call = env.call(stage, "none", messages, Some(grammar.clone()), None, next_step(journal, &env.run_id), false);
        // The decision's free text is shown while it is written (`chatflow.live`).
        call.stream.live = shape.text_fields();
        let called = llm::chat(journal, call, &any).await?;
        match parse_json::<T>(&called.result.content) {
            Ok(decision) => return Ok((decision, called.result.content)),
            Err(error) => {
                last_error = error.clone();
                live = format!("{live}\n{}", env.data.text("parse_retry", &[("error", &error)])).trim().to_owned();
            }
        }
    }
    Err(RpcError::new(codes::INTERNAL, format!("the {stage} decision could not be read: {last_error}")))
}

async fn set_stage(env: &RunEnv, journal: &mut Journal, status_seq: u64, stage: &str) -> Result<(), RpcError> {
    env.set_stage(stage);
    journal.set(status_seq, "stage", json!(stage)).await
}

/// Run (or resume) the foreground turn whose `status` entry is `status_seq`.
pub async fn run_turn(env: &RunEnv, journal: &mut Journal, status_seq: u64) -> Result<TurnEnd, RpcError> {
    agent_loop::repair_interrupted(journal, &env.run_id, &env.data).await?;
    let mut done_reads: HashSet<String> = HashSet::new();
    // Reads already made in this run are never repeated.
    for entry in journal.entries().filter(|entry| str_field(&entry.data, "run_id") == env.run_id) {
        if entry.kind == kinds::TOOL_CALL && entry.data.get("host").and_then(Value::as_bool) == Some(true) {
            let req = ReadReq {
                tool: str_field(&entry.data, "tool").to_owned(),
                args: entry.data.get("args").cloned().unwrap_or(Value::Null),
                intent: String::new(),
                impact: false,
            };
            done_reads.insert(req.key());
        }
    }
    let mut route_name = String::new();
    let mut effort = "medium".to_owned();
    let mut agent_outcome = None;
    loop {
        if env.stops.park.is_cancelled() || env.stops.pause.is_cancelled() {
            return Ok(TurnEnd { state: "parked".into(), detail: String::new(), route: route_name, agent: None });
        }
        if env.stops.cancel.is_cancelled() {
            return Ok(TurnEnd { state: "cancelled".into(), detail: "Cancelled.".into(), route: route_name, agent: None });
        }
        journal.sync().await?;
        let entries = journal.entries_vec();
        // The route and effort decided so far.
        for entry in entries.iter().filter(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "run_id") == env.run_id) {
            if str_field(&entry.data, "stage") == "route" {
                route_name = str_field(&entry.data, "route").to_owned();
            }
            if let Some(level) = entry.data.get("detail").and_then(|detail| detail.get("effort")).and_then(Value::as_str) {
                effort = schemas::effort_or_default(level);
            }
        }
        let stage = derive(&entries, &env.run_id, env.data.limits.read_rounds);
        let outcome: Result<(), RpcError> = match stage.clone() {
            Stage::Done => {
                let (state, detail) = match &agent_outcome {
                    Some(LoopOutcome { outcome, summary, .. }) => (outcome.clone(), summary.clone()),
                    None => ("done".to_owned(), String::new()),
                };
                return Ok(TurnEnd { state, detail, route: route_name, agent: agent_outcome });
            }
            Stage::Route => {
                set_stage(env, journal, status_seq, "route").await?;
                let (mut route_shape, catalog, skills_live) = route_shape(env, &entries)?;
                // The session's background work still going, next to the decision: a question about
                // it is answered from where it stands (seen: "how is it going?" answered with a
                // greeting while eight agents worked).
                let open = crate::manage::open_work(env, &entries).await;
                // A plan that stopped before its tasks were done, with nothing at work: "run the
                // tasks again" starts its tasks as they stand (IMPLEMENT), not a plan written anew
                // (seen: after the agent was cancelled, "run the tasks again" went to DO and the
                // planner wrote the same ten tasks again, then once more for a steer).
                let stopped = if open.is_empty() { plan::stopped(&entries) } else { None };
                // IMPLEMENT needs an open or a stopped plan: otherwise the grammar has none.
                if plan::open(&entries).is_none() && stopped.is_none() {
                    route_shape = route_shape.without("route", "IMPLEMENT");
                }
                // MANAGE needs background work that is going on: with none, there is nothing to
                // stop or steer and the grammar has no such route (seen: "continue working on the
                // tasks" after every agent was stopped went to MANAGE, found nothing open, and the
                // turn ended in a reply that could only say "I will").
                if open.is_empty() {
                    route_shape = route_shape.without("route", "MANAGE");
                }
                let message = journal.entries().filter(|entry| entry.kind == kinds::USER_MESSAGE).last().map(|entry| str_field(&entry.data, "text").to_owned()).unwrap_or_default();
                let work_live = if !open.is_empty() {
                    env.data.text("route_open_work", &[("work", &crate::manage::work_lines(env, &open))])
                } else if let Some((_, stopped)) = &stopped {
                    let left: String = stopped
                        .tasks
                        .iter()
                        .filter(|task| !stopped.steps_done.iter().any(|step| step.n == task.n && step.outcome == "done"))
                        .map(|task| env.data.text("list_item", &[("text", &task.title)]))
                        .collect::<Vec<_>>()
                        .join("\n");
                    env.data.text("route_stopped_plan", &[("tasks", &left)])
                } else if entries.iter().any(|entry| entry.kind == chatflow_kinds::PLAN) {
                    // Said only where a plan was at work in this conversation.
                    env.data.text("route_no_open_work", &[])
                } else {
                    String::new()
                };
                // A short message is shown again as the last thing before the decision: its asks
                // and its route are about these words (seen: "continue" routed by the message
                // before it, "as i said stop" by "so?"; five of 26 turns of one conversation). A
                // long message carries its own asks and is not repeated.
                let latest_live = if message.trim().is_empty() || message.chars().count() > ROUTE_LATEST_CHARS {
                    String::new()
                } else {
                    env.data.text("route_latest", &[("message", message.trim())])
                };
                let skills_live = joined(&[work_live, skills_live, latest_live]);
                let routed = decide_json::<RouterDecision>(env, journal, "ROUTE", &route_shape, &skills_live).await;
                // Every ask quotes words that are not in the latest message: the decision is about
                // another message, so its route is too. Decided once more, told so; a second such
                // decision stands with the message itself as its one ask (below).
                let routed = match routed {
                    Ok((decision, raw)) => {
                        let mut asks = decision.asks.clone();
                        let elsewhere = anchor_asks(&mut asks, &message);
                        if !decision.asks.is_empty() && elsewhere.len() == decision.asks.len() {
                            let again = joined(&[skills_live.clone(), env.data.text("route_other_message", &[])]);
                            match decide_json::<RouterDecision>(env, journal, "ROUTE", &route_shape, &again).await {
                                Ok(pair) => Ok(pair),
                                Err(error) if error.code == codes::CANCELLED => Err(error),
                                Err(_) => Ok((decision, raw)),
                            }
                        } else {
                            Ok((decision, raw))
                        }
                    }
                    Err(error) => Err(error),
                };
                let (decision, raw) = match routed {
                    Ok(pair) => pair,
                    Err(error) if error.code == codes::CANCELLED => continue,
                    // Router failure: read and let DECIDE choose.
                    Err(error) => {
                        let text = journal
                            .entries()
                            .filter(|entry| entry.kind == kinds::USER_MESSAGE)
                            .last()
                            .map(|entry| str_field(&entry.data, "text").to_owned())
                            .unwrap_or_default();
                        let fallback = RouterDecision {
                            asks: if text.trim().is_empty() { Vec::new() } else { vec![schemas::Ask { quote: text.clone(), means: String::new() }] },
                            work: text,
                            route: "LOOKUP".into(),
                            effort: "medium".into(),
                            symbols: vec![],
                            files: vec![],
                            queries: vec![],
                            focus: vec![],
                            skills: vec![],
                            teaches: String::new(),
                            drops: String::new(),
                            direction: String::new(),
                            private: String::new(),
                        };
                        (fallback, format!("router failed: {error}"))
                    }
                };
                let route = if schemas::ROUTES.contains(&decision.route.as_str()) { decision.route.clone() } else { "LOOKUP".into() };
                let mut decision = decision;
                // Kept to the catalog and to the routes whose grammar writes them (a fallback route
                // never inherits a choice).
                decision.skills = if route_shape.live_on("skills", &route) {
                    skillsel::chosen(&decision.skills, &catalog, env.data.limits.skills_per_turn)
                } else {
                    Vec::new()
                };
                let teaches = decision.teaches == "yes";
                let drops = decision.drops == "yes";
                let private = decision.private == "yes";
                // The asks are the user's latest message, never the conversation before it.
                let unanchored = anchor_asks(&mut decision.asks, &message);
                journal
                    .append(
                        kinds::ROUTE,
                        json!({"route": route, "stage": "route", "detail": decision, "raw": raw, "run_id": env.run_id, "teaches": teaches, "drops": drops, "private": private, "asks_dropped": unanchored}),
                    )
                    .await
                    .map(|_| ())
            }
            Stage::Skills { route_seq } => {
                let skills = journal.get(route_seq).map(skillsel::of_decision).unwrap_or_default();
                skillsel::inject(env, journal, route_seq, &skills).await
            }
            Stage::Reads { from_seq } => {
                set_stage(env, journal, status_seq, "read").await?;
                let Some(entry) = journal.get(from_seq).cloned() else { return Err(RpcError::internal("decision entry vanished")) };
                let languages = if reads_diagnostics(&entry, &route_name) { reads::languages(&env.host).await } else { Vec::new() };
                let planned = round_reads(&env.scope, &env.session_id, &entry, &route_name, &env.data.limits, &languages);
                let any = env.stops.any();
                let max = env.data.limits.reads_per_round;
                let ran = reads::run(journal, &env.run_id, &env.scope, planned, &mut done_reads, &any, max).await?;
                knowledge_note(env, journal).await?;
                match ran {
                    (0, 0) => journal
                        .note(&env.run_id, "no_reads", &env.data.text("no_reads", &[]), true)
                        .await
                        .map(|_| ()),
                    (_, 0) => Ok(()),
                    (_, skipped) => journal
                        .note(&env.run_id, "reads_capped", &env.data.text("reads_capped", &[("skipped", &skipped.to_string()), ("max", &max.to_string())]), true)
                        .await
                        .map(|_| ()),
                }
            }
            Stage::StatusReads => {
                set_stage(env, journal, status_seq, "read").await?;
                let planned = status_reads(&env.scope);
                let any = env.stops.any();
                let read = reads::run(journal, &env.run_id, &env.scope, planned, &mut done_reads, &any, env.data.limits.reads_per_round).await.map(|_| ());
                // What each agent this session started is doing now, counted from its own steps:
                // its record says `working` from the moment it starts, whatever it does since
                // (seen: "are you sure it is working?" with nothing to answer it from).
                let entries = journal.entries_vec();
                let work = crate::manage::open_work(env, &entries).await;
                if read.is_ok() && !work.is_empty() {
                    let plan = crate::manage::plan_lines(env, &entries, &work).await;
                    let (lines, _) = crate::manage::work_lines_with_progress(env, &work).await;
                    journal
                        .append(kinds::NOTE, json!({"text": env.data.text("status_work", &[("plan", &plan), ("lines", &lines)]), "purpose": "status_work", "run_id": env.run_id, "host_message": true}))
                        .await?;
                }
                read
            }
            Stage::Manage => {
                set_stage(env, journal, status_seq, "manage").await?;
                crate::manage::run(env, journal).await
            }
            Stage::Commit { route_seq } => {
                set_stage(env, journal, status_seq, "decide").await?;
                if units_delegation(env, journal, route_seq).await? {
                    continue;
                }
                let detail = journal.get(route_seq).and_then(|entry| entry.data.get("detail").cloned()).unwrap_or(Value::Null);
                let list = |field: &str| detail.get(field).cloned().unwrap_or_else(|| json!([]));
                // The files the request itself names are read before the plan (targeted, never a
                // map): a planner that has not seen them plans a task to read them.
                let named: Vec<ReadReq> = detail_list(&journal.get(route_seq).cloned().ok_or_else(|| RpcError::internal("route entry vanished"))?, "files")
                    .iter()
                    .map(|file| file.trim())
                    .filter(|file| !file.is_empty() && std::path::Path::new(&env.scope.workspace).join(file).is_file())
                    .map(|file| reads::file_read_for(&env.scope, &env.session_id, file, str_field(&detail, "work")))
                    .collect();
                if !named.is_empty() {
                    let any = env.stops.any();
                    let mut fresh: HashSet<String> = HashSet::new();
                    reads::run(journal, &env.run_id, &env.scope, named, &mut fresh, &any, env.data.limits.reads_per_round).await?;
                }
                knowledge_note(env, journal).await?;
                let decision = json!({
                    "missing": "",
                    "route": "DO",
                    "effort": schemas::effort_or_default(str_field(&detail, "effort")),
                    "symbols": list("symbols"),
                    "files": list("files"),
                    "queries": [],
                    "brief": "",
                    "checks": [],
                    "skills": [],
                    "note": "",
                    "by": "host",
                });
                journal.append(kinds::ROUTE, json!({"route": "DO", "stage": "decide", "detail": decision, "run_id": env.run_id})).await.map(|_| ())
            }
            Stage::Decide { allow_more } => {
                set_stage(env, journal, status_seq, "decide").await?;
                // After the reads DECIDE may add a skill the request did not call for (the reads
                // showed an SQL query, a borrow error): the catalog minus what the turn has.
                let used = skillsel::used_by_run(&entries, &env.run_id);
                let catalog = decide_catalog(&env.prefix.skills, &used);
                let max = env.data.limits.skills_per_turn.min(2);
                let full = decide_routes(stage_shape(env, "DECIDE")?, &route_name).with_catalog(&catalog, max);
                let (shape, mut live) = if allow_more {
                    (full, String::new())
                } else {
                    (full.without("route", "MORE"), env.data.text("decide_no_more", &[]))
                };
                let skills_text = skills_live(&env.data, &env.prefix, &catalog, &[]);
                if !skills_text.is_empty() {
                    live = if live.is_empty() { skills_text } else { format!("{skills_text}\n{live}") };
                }
                // `checks` is a pick of the project's own check commands (none: no checks).
                let candidates = crate::verifiers::candidates(env, &run_files(&entries, &env.run_id)).await;
                let shape = shape.filled(schemas::VERIFIERS_PLACEHOLDER, &crate::verifiers::commands(&candidates), None);
                let candidates = crate::verifiers::candidates_text(&env.data, &candidates);
                if !candidates.is_empty() {
                    live = if live.is_empty() { candidates } else { format!("{live}\n{candidates}") };
                }
                match decide_json::<Decision>(env, journal, "DECIDE", &shape, &live).await {
                    Ok((mut decision, raw)) => {
                        if !schemas::DECISIONS.contains(&decision.route.as_str()) || (!allow_more && decision.route == "MORE") {
                            decision.route = if route_name == "LOOKUP" { "ANSWER".into() } else { route_name.clone() };
                        }
                        decision.effort = schemas::effort_or_default(&decision.effort);
                        decision.skills = skillsel::chosen(&decision.skills, &catalog, max);
                        let added = decision.skills.clone();
                        let seq = journal
                            .append(kinds::ROUTE, json!({"route": decision.route, "stage": "decide", "detail": decision, "raw": raw, "run_id": env.run_id}))
                            .await?;
                        // Into the conversation now, before whatever the decision runs (the Act
                        // stage injects the same seq again: a no-op once this note exists).
                        skillsel::inject(env, journal, seq, &added).await
                    }
                    Err(error) => Err(error),
                }
            }
            Stage::Reply => {
                set_stage(env, journal, status_seq, "reply").await?;
                // An answer in a session with background work going says where that work stands
                // from its records, not from memory of the conversation.
                let open = crate::manage::open_work(env, &entries).await;
                let mut reply_live = if open.is_empty() { String::new() } else { env.data.text("reply_open_work", &[("work", &crate::manage::work_lines(env, &open))]) };
                // What the plan left out on the way is told, not hidden behind "done" (seen: the
                // part that steered the model dropped in a rewrite, the work reported finished).
                let dropped = dropped_tasks(&entries, &env.run_id);
                if !dropped.is_empty() {
                    let told = env.data.text("reply_dropped", &[("tasks", &dropped.join("\n"))]);
                    reply_live = [reply_live.as_str(), told.as_str()].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n");
                }
                // Work this turn did is reported from the host's records of it: each task as it
                // ended, the checks' last result, each ask as ACCEPT judged it (seen: "all seven
                // tasks are done and green" written from the tasks' own closing words, with no
                // check run on any of them).
                // A plan the agents carried out, judged by this run when its tree finished, is
                // reported the same way.
                let recorded = work_outcome(&env.data, &entries, &env.run_id);
                let recorded = if recorded.is_empty() { tree_outcome(&env.data, &entries, &env.run_id) } else { recorded };
                if !recorded.is_empty() {
                    reply_live = [reply_live.as_str(), recorded.as_str()].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n");
                }
                // The questions last: the text right before the answer is what it answers.
                let asks = asks_live(&env.data, &entries, &env.run_id, "reply_asks", true);
                if !asks.is_empty() {
                    reply_live = [reply_live.as_str(), asks.as_str()].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n");
                }
                let messages = crate::fit::fitted(env, journal, |entries| {
                    let mut messages = base_messages(env, entries);
                    messages.push(prompts::stage_message(&env.data, &env.prefix.role, "REPLY", &reply_live));
                    messages
                })
                .await?;
                // A reply runs no tools. They stay listed, so the system block matches the other
                // calls' and is read from the cache; `none` makes the model plugin refuse the
                // tool-call token (listed and allowed, the model wrote a tool call into the answer).
                let call = env.call("REPLY", &effort, messages, None, Some(json!("none")), next_step(journal, &env.run_id), true);
                let any = env.stops.any();
                llm::chat(journal, call, &any).await.map(|_| ())
            }
            Stage::Act { decision_seq, step } => {
                set_stage(env, journal, status_seq, "act").await?;
                let decision: Decision = journal
                    .get(decision_seq)
                    .and_then(|entry| entry.data.get("detail").cloned())
                    .and_then(|detail| serde_json::from_value(detail).ok())
                    .ok_or_else(|| RpcError::internal("the DO decision could not be read back"))?;
                // The run's plan, carried out one task per loop, in order. Its tasks are memory
                // tasks (made once: the board and the cards show them with their state); the loop
                // gets its task, with what the earlier ones did, and the task's contract (the last
                // one's joined by the decision's checks) is verified before the next starts.
                let mut planned = step.and_then(|n| plan::for_run(&entries, &env.run_id).map(|(seq, plan)| (seq, plan, n)));
                let (task, step_spec) = match planned.as_mut() {
                    Some((plan_seq, plan, n)) => {
                        // The plan's checks stand only after the same baseline the foreground
                        // path runs (CHECKS drops a check that cannot judge the work, e.g. a
                        // test runner with no tests in this workspace); run once per run, then
                        // each task keeps the checks that stood.
                        let mut proposed: Vec<String> = decision.checks.clone();
                        for task in plan.tasks.iter().filter(|task| task.task_id.is_none()) {
                            for check in &task.checks {
                                if !proposed.contains(check) {
                                    proposed.push(check.clone());
                                }
                            }
                        }
                        let stood: Vec<String> = if proposed.is_empty() {
                            Vec::new()
                        } else {
                            contract::commands(&crate::checks::refine(env, journal, &contract::contract_from(&proposed)).await?)
                        };
                        for task in plan.tasks.iter_mut().filter(|task| task.task_id.is_none()) {
                            task.checks.retain(|check| stood.contains(check));
                        }
                        // What the baseline revised (new commands) joins the last task.
                        let last_checks: Vec<String> = stood.iter().filter(|check| !plan.tasks.iter().any(|task| task.checks.contains(check))).cloned().collect();
                        materialize_plan_tasks(env, journal, *plan_seq, plan, &last_checks).await?;
                        let planned_task = plan.tasks.get(*n as usize - 1).cloned().unwrap_or_default();
                        let task_id = planned_task.task_id.clone().ok_or_else(|| RpcError::internal("the plan's task was not made"))?;
                        let task: Task = env
                            .host
                            .get::<Task>(memory_methods::TASK_UPDATE, json!({"id": task_id, "status": "in_progress", "session_id": env.session_id, "freeze": true}))
                            .await?;
                        let text = env.data.text("do_task_text", &[("title", &planned_task.title), ("brief", &plan::task_focus(&env.data, &planned_task))]);
                        let later: Vec<String> = plan.tasks.iter().filter(|task| task.n > *n).map(|task| task.title.clone()).collect();
                        let goal = plan.doc.goal.clone();
                        let direction = direction(&env.data, &journal.entries_vec());
                        // The user's own words for the work open every step of it: the plan's goal
                        // and the task's brief are the planner's wording of them.
                        let asks = work_asks(&journal.entries_vec(), &env.run_id, Some(&plan.plan_id));
                        // Where this task's work starts in the transcript (the chat nests what
                        // follows under it); not a message for the model.
                        let label = env.data.text("task_start", &[("n", &n.to_string()), ("of", &plan.tasks.len().to_string()), ("title", &planned_task.title)]);
                        let started = journal.entries().any(|entry| {
                            entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == env.run_id && str_field(&entry.data, "purpose") == "task_start" && str_field(&entry.data, "task_id") == task_id
                        });
                        if !started {
                            journal
                                .append(kinds::NOTE, json!({"text": label, "purpose": "task_start", "run_id": env.run_id, "host_message": false, "task_id": task_id, "n": *n, "of": plan.tasks.len(), "plan_id": plan.plan_id}))
                                .await?;
                            // The task starts with its own discovery, made by the host as the model's
                            // own reads: a semantic search of the code graph for the task's topic, and
                            // the files it touches as they are now (no reason to read them again).
                            let topic = format!("{} {}", planned_task.title, planned_task.brief);
                            let mut files: Vec<ReadReq> = vec![reads::graph_search(&env.scope, topic.trim(), env.data.limits.search_limit)];
                            // A file already read in this run and not changed since is in view: no
                            // second copy (the plan's read of the files the request names).
                            let in_view = |file: &str| {
                                let entries = journal.entries_vec();
                                let read_at = entries
                                    .iter()
                                    .filter(|entry| entry.kind == kinds::TOOL_CALL && str_field(&entry.data, "run_id") == env.run_id)
                                    .filter(|entry| entry.data.get("args").and_then(|args| args.get("target").or_else(|| args.get("path"))).and_then(Value::as_str) == Some(file))
                                    .map(|entry| entry.seq)
                                    .max();
                                read_at.is_some_and(|at| {
                                    !entries.iter().any(|entry| {
                                        entry.seq > at
                                            && entry.kind == kinds::TOOL_RESULT
                                            && entry.data.get("changed_files").and_then(Value::as_array).is_some_and(|changed| changed.iter().any(|path| path.as_str() == Some(file)))
                                    })
                                })
                            };
                            files.extend(
                                planned_task
                                    .touches
                                    .iter()
                                    .filter(|item| std::path::Path::new(&env.scope.workspace).join(item.as_str()).is_file() && !in_view(item))
                                    .map(|file| reads::file_read_for(&env.scope, &env.session_id, file, &format!("{} {}", planned_task.title, planned_task.brief))),
                            );
                            // A function or class the task touches is read whole (a long file's read
                            // shows only its outline). A touch is a file name when it has a path
                            // separator or an extension a language plugin declares; else a symbol.
                            let languages = reads::declared_languages(&env.host).await;
                            let is_file_name = |item: &str| {
                                item.contains('/')
                                    || std::path::Path::new(item)
                                        .extension()
                                        .and_then(|ext| ext.to_str())
                                        .is_some_and(|ext| languages.iter().any(|language| language.extensions.iter().any(|known| known == ext)))
                                    || languages.iter().any(|language| language.filenames.iter().any(|known| known == item))
                            };
                            files.extend(
                                planned_task
                                    .touches
                                    .iter()
                                    // `file.py:name` and `file.rs::name` name a symbol in a file.
                                    .map(|item| item.trim().rsplit_once(':').map(|(_, name)| name).unwrap_or(item.trim()).trim())
                                    .filter(|item| !item.is_empty() && !is_file_name(item) && !std::path::Path::new(&env.scope.workspace).join(item).exists())
                                    .map(|name| reads::file_read(&env.scope, &env.session_id, &format!("symbol://{name}"))),
                            );
                            if !files.is_empty() {
                                let any = env.stops.any();
                                let mut fresh: HashSet<String> = HashSet::new();
                                reads::run(journal, &env.run_id, &env.scope, files, &mut fresh, &any, env.data.limits.reads_per_round).await?;
                            }
                        }
                        (Some(task), Some(agent_loop::StepSpec { n: *n, of: plan.tasks.len() as u32, text, done: plan.steps_done.clone(), goal, later, direction, asks }))
                    }
                    None => (foreground_task(env, journal, &decision).await?, None),
                };
                let last = planned.as_ref().is_none_or(|(_, plan, n)| *n as usize >= plan.tasks.len());
                // The decision's own skills: what DECIDE added after the reads (injected when it was
                // made; a no-op here), or an IMPLEMENT decision's plan task skills.
                skillsel::inject(env, journal, decision_seq, &decision.skills).await?;
                // A plan task is execution: the thinking was the plan's; the loop thinks at
                // `effort.task` (short thoughts, the work in tool calls).
                let effort = if step_spec.is_some() { env.data.effort.task.clone() } else { schemas::effort_or_default(&decision.effort) };
                let spec = LoopSpec { kind: LoopKind::Foreground, effort, task, step: step_spec };
                match agent_loop::run(env, journal, &spec).await {
                    Ok(outcome) if outcome.outcome == "parked" || outcome.outcome == "cancelled" => {
                        return Ok(TurnEnd { state: outcome.outcome.clone(), detail: outcome.summary.clone(), route: route_name, agent: Some(outcome) });
                    }
                    Ok(outcome) => {
                        // The step is recorded on the plan (the card shows it; a resumed run goes on
                        // from the next); a step that ended well and is not the last ends here, and
                        // the next loop takes the following step.
                        if let Some((plan_seq, plan, n)) = planned {
                            // The user wrote during the task: the planner writes the tasks still to
                            // do (this one included) from what they ask now.
                            if outcome.outcome == "steered" {
                                // A question about how the work goes is answered from the plan and
                                // the task goes on (seen: "how is it going?" rewrote the plan, and
                                // the new first task was a unit test the user never asked for).
                                if steer_is_question(env, journal, &plan, n, &outcome.summary).await {
                                    answer_progress(env, journal, &plan, n).await?;
                                    continue;
                                }
                                set_stage(env, journal, status_seq, "plan").await?;
                                write_plan(env, journal, PlanMode::Replan { plan_seq, failed: n, outcome: outcome.outcome.clone(), summary: outcome.summary.clone() }).await?;
                                continue;
                            }
                            // A task whose work is verified as it stands (a command that passed
                            // since its last edit) is done, also when its loop ended with nothing
                            // left to do or was stopped for re-running that same passing check.
                            let task_id = plan.tasks.get(n as usize - 1).and_then(|task| task.task_id.clone()).unwrap_or_default();
                            // Only a check counts: the task's own, or one of the project's check
                            // commands (looked up only when the loop ended in a way this decides).
                            // A task stopped while the host's own checks kept failing is not made
                            // done by a narrower run of its own that passed.
                            let promotable = outcome.verified != Some(false) && (outcome.outcome == "stopped" || (outcome.outcome == "done" && outcome.tool_calls == 0));
                            let verified = promotable && {
                                let entries = journal.entries_vec();
                                let mut checks: Vec<String> = plan.tasks.get(n as usize - 1).map(|task| task.checks.clone()).unwrap_or_default();
                                checks.extend(crate::verifiers::commands(&crate::verifiers::candidates(env, &run_files(&entries, &env.run_id)).await));
                                verified_since_last_edit(&entries, &env.run_id, &task_id, &checks)
                            };
                            let outcome = if verified {
                                agent_loop::LoopOutcome { outcome: "done".into(), tool_calls: outcome.tool_calls.max(1), ..outcome.clone() }
                            } else {
                                outcome.clone()
                            };
                            // A task whose loop made no tool call did nothing: the model is told
                            // so once and the task runs again; a second time it counts as failed.
                            if outcome.outcome == "done" && outcome.tool_calls == 0 {
                                let nudged = journal.entries().any(|entry| {
                                    entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == env.run_id && str_field(&entry.data, "purpose") == "task_nudge" && entry.data.get("n").and_then(Value::as_u64) == Some(n as u64)
                                });
                                let title = plan.tasks.get(n as usize - 1).map(|task| task.title.clone()).unwrap_or_default();
                                if !nudged {
                                    journal
                                        .append(kinds::NOTE, json!({"text": env.data.text("task_nudge", &[("n", &n.to_string()), ("title", &title)]), "purpose": "task_nudge", "run_id": env.run_id, "host_message": true, "n": n}))
                                        .await?;
                                    continue;
                                }
                            }
                            let did_nothing = outcome.outcome == "done" && outcome.tool_calls == 0;
                            let outcome_name = if did_nothing { "failed".to_owned() } else { outcome.outcome.clone() };
                            let mut done = plan.steps_done.clone();
                            // What the task established (its notebook's Known: the names it made
                            // and where they are) travels with its report: the next task's step
                            // shows only the reports, so a label or edge one task wrote was guessed
                            // by the next (seen: "the rel in the graph is probably YIELDS_FROM").
                            let summary = match crate::projection::notebook_known(&journal.entries_vec()) {
                                Some(known) => env.data.text("task_report_known", &[("summary", outcome.summary.trim()), ("known", &known)]),
                                None => outcome.summary.clone(),
                            };
                            done.push(plan::StepDone {
                                n,
                                outcome: outcome_name.clone(),
                                summary,
                                changed_files: outcome.changed_files.clone(),
                                check: outcome.check.clone(),
                                check_detail: outcome.check_detail.clone(),
                            });
                            journal.set(plan_seq, "steps_done", serde_json::to_value(&done)?).await?;
                            // The memory task's state follows the loop (a verified contract set it
                            // already; a task without one is done when its loop ended well).
                            if let Some(task_id) = plan.tasks.get(n as usize - 1).and_then(|task| task.task_id.clone()) {
                                let status = if outcome_name == "done" { "done" } else { "failed" };
                                if let Err(error) = env.host.get::<Task>(memory_methods::TASK_UPDATE, json!({"id": task_id, "status": status})).await {
                                    tracing::warn!(%error, task = %task_id, "the plan task's status could not be recorded");
                                }
                            }
                            if !last && outcome_name == "done" {
                                // Before the next task: do the tasks still to do fit what this one
                                // showed (an error elsewhere, a wrong assumption)? If not, they are
                                // written again now, not after one of them fails.
                                if plan.reviews < env.data.limits.reviews
                                    && let Some(found) = review_plan(env, journal, &plan, n).await
                                {
                                    set_stage(env, journal, status_seq, "plan").await?;
                                    write_plan(env, journal, PlanMode::Replan { plan_seq, failed: n, outcome: outcome_name.clone(), summary: found }).await?;
                                }
                                // The next task starts from its own context: this task's steps
                                // become one note (its calls, the files it changed, its last test
                                // run), and the next step message lists its reply under "Done so
                                // far" (seen: five tasks of one plan each worked inside every earlier
                                // task's reads, edits and test logs, the prompt grown past 190k).
                                crate::fit::fold_steps(journal).await?;
                                continue;
                            }
                            // The last task ended well: before the work is reported, each ask of
                            // the user is checked against what the work's runs printed, and an ask
                            // not shown met writes the tasks that meet it (seen: a search system
                            // reported done with its OCR and models switched off, scored against
                            // the answer key it read its answers from).
                            // The check always runs and its verdict is recorded (the report of the
                            // work tells each ask as it stands); the work goes on for an unmet ask
                            // within `accepts`, its own count (seen: the replans spent on reviews,
                            // and the work reported done with no look at what was asked).
                            if last
                                && outcome_name == "done"
                                && let Some(unmet) = accept_plan(env, journal, plan_seq).await
                                && plan.accepts < env.data.limits.accepts
                            {
                                set_stage(env, journal, status_seq, "plan").await?;
                                if write_plan(env, journal, PlanMode::Replan { plan_seq, failed: n, outcome: "unmet".into(), summary: unmet }).await? {
                                    crate::fit::fold_steps(journal).await?;
                                    continue;
                                }
                            }
                            // A task that ended badly: the plan is written again from the first
                            // task not done (the done ones stay), a bounded number of times, and
                            // the work goes on with it.
                            if outcome_name != "done" && plan.replans < env.data.limits.replans {
                                // A stuck task starts fresh: its attempt leaves the context and one
                                // note says what did not work, so the next approach is not built on the
                                // stuck attempt's picture of the code.
                                if outcome_name == "stuck" {
                                    let title = plan.tasks.get(n as usize - 1).map(|task| task.title.clone()).unwrap_or_default();
                                    fresh_start(env, journal, &task_id, n, &title, &outcome.summary).await?;
                                }
                                set_stage(env, journal, status_seq, "plan").await?;
                                // Written: the work goes on with the new tasks. Refused (a runner
                                // job): the run ends here with this task as it ended.
                                if write_plan(env, journal, PlanMode::Replan { plan_seq, failed: n, outcome: outcome_name.clone(), summary: outcome.summary.clone() }).await? {
                                    continue;
                                }
                            }
                            journal.set(plan_seq, "status", json!(if outcome_name == "done" { plan_status::DONE } else { plan_status::IMPLEMENTING })).await?;
                        }
                        let dropped = dropped_note(&env.data, &journal.entries_vec(), &env.run_id);
                        journal
                            .append(
                                kinds::NOTE,
                                json!({
                                    "text": format!("{}{dropped}", act_note(&outcome)),
                                    "purpose": "act_done",
                                    "run_id": env.run_id,
                                    "host_message": false,
                                    "outcome": outcome.outcome,
                                    "changed_files": outcome.changed_files,
                                    "verified": outcome.verified,
                                    "check": outcome.check,
                                    "check_detail": outcome.check_detail,
                                    "summary": outcome.summary,
                                }),
                            )
                            .await?;
                        // The run goes into the graph (docs/PROJECT-KNOWLEDGE-PLAN.md P5), with the
                        // task it worked for when the decision made one.
                        let linked_task = journal
                            .entries()
                            .filter(|entry| entry.kind == kinds::TASK_LINK && str_field(&entry.data, "run_id") == env.run_id)
                            .map(|entry| str_field(&entry.data, "task_id").to_owned())
                            .last();
                        let _ = crate::afteraction::record_run(env, journal, linked_task.as_deref(), &outcome).await;
                        // A plan carried out by this loop is done, or open again when the work was not.
                        let plan_id = journal.get(decision_seq).map(|entry| str_field(&entry.data, "plan_id").to_owned()).unwrap_or_default();
                        if !plan_id.is_empty()
                            && let Some((seq, _)) = plan::version_of(&journal.entries_vec(), &plan_id)
                        {
                            let status = if outcome.outcome == "done" { plan_status::DONE } else { plan_status::APPROVED };
                            journal.set(seq, "status", json!(status)).await?;
                        }
                        agent_outcome = Some(outcome);
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            Stage::Diagnostics { route_seq } => {
                set_stage(env, journal, status_seq, "read").await?;
                let Some(entry) = journal.get(route_seq).cloned() else { return Err(RpcError::internal("route entry vanished")) };
                let languages = reads::languages(&env.host).await;
                let planned = lookup_reads(&env.scope, &env.session_id, &entry, &languages);
                let any = env.stops.any();
                match reads::run(journal, &env.run_id, &env.scope, planned, &mut done_reads, &any, env.data.limits.reads_per_round).await {
                    // Nothing named is a file: the stage ends with a transcript note.
                    Ok((0, _)) => journal.note(&env.run_id, "diagnostics_read", &env.data.text("diagnostics_none", &[]), false).await.map(|_| ()),
                    Ok(_) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            Stage::Explore => {
                set_stage(env, journal, status_seq, "explore").await?;
                if let Some(route_seq) = entries.iter().rev().find(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "run_id") == env.run_id && str_field(&entry.data, "stage") == "route").map(|entry| entry.seq)
                    && units_delegation(env, journal, route_seq).await?
                {
                    continue;
                }
                let spec = LoopSpec { kind: LoopKind::Question, effort: effort.clone(), task: None, step: None };
                match agent_loop::run(env, journal, &spec).await {
                    Ok(outcome) if outcome.outcome == "parked" || outcome.outcome == "cancelled" => {
                        return Ok(TurnEnd { state: outcome.outcome.clone(), detail: outcome.summary.clone(), route: route_name, agent: Some(outcome) });
                    }
                    Ok(outcome) => {
                        // A skill that drives a procedure through a tool says when its work is
                        // finished (`SkillInfo::done`): while it says the work is open, the turn goes
                        // on with the command's output as the latest note, whose last line is the
                        // next step (seen: an audit of 15 units ended at the first unit with a
                        // progress report, twice). The steps so far fold into one note first, so
                        // each piece of the work starts from a short history (the tool keeps the
                        // state) instead of the engine re-reading every earlier piece. It goes on
                        // only while the output changes between rounds: the same output twice means
                        // the model made no progress, and the turn ends with what there is.
                        let open = if outcome.outcome == "done" { work_open(env, journal).await? } else { None };
                        if let Some(open) = open {
                            crate::fit::fold_steps(journal).await?;
                            journal
                                .append(kinds::NOTE, json!({"text": env.data.text("work_open", &[("output", open.trim())]), "purpose": "work_open", "output": open.trim(), "run_id": env.run_id, "host_message": true}))
                                .await?;
                        } else {
                            // The message was answered as a question while agents carry a plan
                            // out: when it asks something of that work, the plan takes it up.
                            if outcome.outcome == "done" && matches!(route_name.as_str(), "LOOKUP" | "ANSWER") {
                                asks_reach_plan(env, journal).await?;
                            }
                            journal
                                .append(
                                    kinds::NOTE,
                                    // An answer that used no tool and changed nothing has no work to count.
                                    json!({"text": if outcome.tool_calls == 0 && outcome.changed_files.is_empty() && outcome.outcome == "done" { String::new() } else { act_note(&outcome) }, "purpose": "explore_done", "run_id": env.run_id, "host_message": false, "outcome": outcome.outcome}),
                                )
                                .await?;
                            agent_outcome = Some(outcome);
                        }
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            Stage::Delegate { decision_seq } => {
                set_stage(env, journal, status_seq, "delegate").await?;
                let detail = journal.get(decision_seq).and_then(|entry| entry.data.get("detail").cloned()).unwrap_or(Value::Null);
                let route = entries.iter().rev().find(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "run_id") == env.run_id && str_field(&entry.data, "stage") == "route");
                let work = route.and_then(|entry| entry.data.get("detail")).map(|detail| str_field(detail, "work").to_owned()).unwrap_or_default();
                let brief = str_field(&detail, "brief");
                let goal = if brief.trim().is_empty() { work.clone() } else { brief.to_owned() };
                let missing = str_field(&detail, "missing");
                let mut context = env.data.text("delegate_context", &[("work", &work), ("missing", missing)]);
                // ROUTE's skills go with the work: the agent's run reads them first.
                let skills = route.map(skillsel::of_decision).unwrap_or_default();
                if !skills.is_empty() {
                    context.push_str(&env.data.text("delegate_skills", &[("uris", &skills.join("\n"))]));
                }
                let params = json!({"project_id": env.scope.project_id, "from_session": env.session_id, "goal": goal, "context": context, "mode": env.mode});
                let any = env.stops.any();
                match env.host.call_with(agents_methods::DELEGATE, params, crate::host::Effect::Effect, Some(&any)).await {
                    Ok(value) => {
                        let text = match serde_json::from_value::<DelegateResult>(value) {
                            Ok(result) => delegated_note(&env.data, &result),
                            Err(error) => env.data.text("delegation_unreadable", &[("error", &error.to_string())]),
                        };
                        journal.note(&env.run_id, "delegated", &text, true).await.map(|_| ())
                    }
                    Err(error) if error.code == codes::CANCELLED => continue,
                    // Nothing was started: the work goes on here, planned and carried out as a DO
                    // decision, instead of a reply that promises it for a later turn (seen: "on
                    // the next turn I will…", and the user had to ask why nothing happened).
                    Err(error) => {
                        journal.note(&env.run_id, "delegated", &env.data.text("delegation_failed", &[("error", &error.to_string())]), true).await?;
                        let effort = schemas::effort_or_default(str_field(&detail, "effort"));
                        let decision = json!({
                            "missing": "",
                            "route": "DO",
                            "effort": effort,
                            "symbols": [],
                            "files": detail.get("files").cloned().unwrap_or_else(|| json!([])),
                            "queries": [],
                            "brief": goal,
                            "checks": detail.get("checks").cloned().unwrap_or_else(|| json!([])),
                            "skills": [],
                            "note": "",
                            "by": "host",
                        });
                        journal.append(kinds::ROUTE, json!({"route": "DO", "stage": "decide", "detail": decision, "run_id": env.run_id})).await.map(|_| ())
                    }
                }
            }
            Stage::Plan { auto, .. } => {
                set_stage(env, journal, status_seq, "plan").await?;
                // A lead session's plan is written by a frontier planning agent (`write_plan`),
                // and every one of its tasks goes to agents (`parallel_implementation`).
                run_plan(env, journal, auto).await?;
                if auto {
                    parallel_implementation(env, journal).await?;
                }
                Ok(())
            }
            Stage::Implement { route_seq } => {
                set_stage(env, journal, status_seq, "implement").await?;
                implement(env, journal, route_seq).await
            }
            Stage::Knowledge { route_seq } => {
                set_stage(env, journal, status_seq, "knowledge").await?;
                propose_knowledge(env, journal, route_seq).await
            }
            Stage::SetAside { route_seq } => set_aside(env, journal, route_seq).await,
        };
        match outcome {
            Ok(()) => {}
            Err(error) if error.code == codes::CANCELLED => continue,
            Err(error) => return Err(error),
        }
    }
}

/// The skills DECIDE may add after the reads: the session's catalog minus what the turn has
/// (ROUTE's choice, skills read so far). A read may show the work is something the request did
/// not say (an SQL query behind a wrong count, a borrow error behind "cargo build fails").
fn decide_catalog(catalog: &[String], used: &[String]) -> Vec<String> {
    catalog.iter().filter(|uri| !used.contains(uri)).cloned().collect()
}

/// DECIDE's routes after ROUTE's: a request ROUTE read as a plan ("plan it, don't change code
/// yet") stays one: DECIDE may read more, plan or answer, never turn it into a change
///. Measured: a plan request became DO on the 27B.
/// A stuck task's attempt leaves the context: one host note says what did not work (each command
/// that still failed, with the end of its last output) and marks the attempt's entries, which the
/// projection drops from then on. The next attempt does not start as the last one did: the note
/// carries what earlier work learned from those failures (`learn::at_failure` by each failure's
/// key) and the skills the attempt's errors brought (`display.skills`, gone with its steps).
async fn fresh_start(env: &RunEnv, journal: &mut Journal, task_id: &str, n: u32, title: &str, summary: &str) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    let Some(from) = entries
        .iter()
        .find(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == env.run_id && str_field(&entry.data, "purpose") == "task_start" && str_field(&entry.data, "task_id") == task_id)
        .map(|entry| entry.seq)
    else {
        return Ok(());
    };
    let to = entries.last().map(|entry| entry.seq).unwrap_or(from);
    let calls: HashMap<&str, &JournalEntry> = entries
        .iter()
        .filter(|entry| entry.seq > from && entry.kind == kinds::TOOL_CALL && str_field(&entry.data, "tool") == "run")
        .map(|entry| (str_field(&entry.data, "call_id"), entry))
        .collect();
    // The latest outcome of each command: only those still failing at the end are listed.
    let mut last: Vec<(String, Option<String>)> = Vec::new();
    for result in entries.iter().filter(|entry| entry.seq > from && entry.kind == kinds::TOOL_RESULT) {
        let Some(call) = calls.get(str_field(&result.data, "call_id")) else { continue };
        let command = call.data.get("args").and_then(|args| args.get("command")).and_then(Value::as_str).unwrap_or("").trim().to_owned();
        let exit = result.data.get("display").and_then(|display| display.get("exit_code")).and_then(Value::as_i64);
        // A status the tools plugin hid (`exit_hidden`: other commands ran after the check, so the
        // status was theirs) shows neither a pass nor a failure: the command is not listed.
        let hidden = result.data.get("display").and_then(|display| display.get("exit_hidden")).and_then(Value::as_bool) == Some(true);
        let failed = (exit != Some(0) && !hidden).then(|| {
            let output = str_field(&result.data, "output");
            let lines: Vec<&str> = output.lines().collect();
            lines[lines.len().saturating_sub(12)..].join("\n")
        });
        last.retain(|(known, _)| known != &command);
        last.push((command, failed));
    }
    // The next attempt does not start as the last one did: what was learned from these failures
    // before, and the skills their errors brought, which go with the dropped steps.
    let languages: Vec<String> = journal
        .context_entry()
        .and_then(|entry| entry.data.get("languages").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|language| language.as_str().map(str::to_owned))
        .collect();
    let mut learned: Vec<Value> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some((arm, lesson, _)) = crate::replay::arm(&entries)
        && arm == "without"
    {
        seen.insert(lesson);
    }
    for output in last.iter().filter_map(|(_, failed)| failed.as_deref()) {
        let Some(key) = crate::learn::failure_key("run", "", output) else { continue };
        for (item, project) in crate::learn::at_failure(&env.host, &env.data, &env.scope.project_id, &key, &languages, &seen).await {
            seen.insert(item.id.clone());
            lines.push(env.data.text("list_item", &[("text", item.text.trim())]));
            learned.push(json!({"id": item.id, "project": project, "key": key}));
        }
    }
    let mut extra = if lines.is_empty() { String::new() } else { env.data.text("attempt_learned", &[("text", &lines.join("\n"))]) };
    let mut skills: Vec<String> = Vec::new();
    for result in entries.iter().filter(|entry| entry.seq > from && entry.kind == kinds::TOOL_RESULT) {
        for uri in result.data.get("display").and_then(|display| display.get("skills")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !skills.iter().any(|known| known == uri) {
                skills.push(uri.to_owned());
            }
        }
    }
    if let Some((first, rest)) = skills.split_first() {
        if let Some((category, name)) = first.strip_prefix("skill://").and_then(|rest| rest.split_once('/'))
            && let Ok(skill) = env.host.get::<Value>(&format!("{category}.skill"), json!({"name": name})).await
        {
            extra.push_str(&env.data.text("attempt_skill", &[("uri", first), ("text", str_field(&skill, "text").trim())]));
        }
        if !rest.is_empty() {
            let uris: Vec<String> = rest.iter().map(|uri| env.prefix.skill_line(uri)).collect();
            extra.push_str(&env.data.text("attempt_skills_also", &[("uris", &uris.join("\n"))]));
        }
    }
    let failures: Vec<String> = last
        .into_iter()
        .filter_map(|(command, failed)| failed.map(|output| env.data.text("attempt_failure", &[("command", &command), ("output", &output)])))
        .collect();
    let failures = if failures.is_empty() { env.data.text("attempt_no_failure", &[]) } else { failures.join("\n") };
    let text = env.data.text("attempt_failed", &[("n", &n.to_string()), ("title", title), ("summary", summary.trim()), ("failures", &failures), ("learned", &extra)]);
    journal
        .append(
            kinds::NOTE,
            json!({"text": text, "purpose": "attempt_failed", "run_id": env.run_id, "host_message": true, "task_id": task_id, "drop_from": from, "drop_to": to, "learned": learned, "skills": skills}),
        )
        .await?;
    Ok(())
}

/// REVIEW after plan task `n` ended well: what the work showed that the tasks still to do do not
/// expect, when the model says they must change. A REVIEW that cannot be read changes nothing.
async fn review_plan(env: &RunEnv, journal: &mut Journal, plan: &plan::Plan, n: u32) -> Option<String> {
    let shape = env.data.shape("REVIEW").cloned()?;
    let title = plan.tasks.get(n as usize - 1).map(|task| task.title.clone()).unwrap_or_default();
    let rest: Vec<String> = plan
        .tasks
        .iter()
        .filter(|task| task.n > n)
        .map(|task| env.data.text("review_task", &[("n", &task.n.to_string()), ("title", &task.title), ("brief", task.brief.trim())]))
        .collect();
    let live = env.data.text("review_live", &[("n", &n.to_string()), ("title", &title), ("tasks", &rest.join("\n"))]);
    match decide_json::<schemas::Review>(env, journal, "REVIEW", &shape, &live).await {
        Ok((review, _)) => {
            let found = review.found.trim();
            (review.replan.trim() == "yes" && !found.is_empty()).then(|| found.to_owned())
        }
        Err(error) => {
            tracing::warn!(%error, "the REVIEW decision could not be read; the plan stays");
            None
        }
    }
}

/// STEER: whether what the user wrote during task `n` only asks how the work goes. A decision that
/// cannot be read counts as asking something of the work (the planner hears the words).
async fn steer_is_question(env: &RunEnv, journal: &mut Journal, plan: &plan::Plan, n: u32, text: &str) -> bool {
    let Some(shape) = env.data.shape("STEER").cloned() else { return false };
    let title = plan.tasks.get(n as usize - 1).map(|task| task.title.clone()).unwrap_or_default();
    let live = env.data.text("steer_live", &[("n", &n.to_string()), ("title", &title), ("text", text.trim())]);
    match decide_json::<schemas::Steer>(env, journal, "STEER", &shape, &live).await {
        Ok((steer, _)) => steer.kind.trim() == "question",
        Err(error) => {
            tracing::warn!(%error, "the STEER decision could not be read; the plan is written again");
            false
        }
    }
}

/// The user wrote while agents carried a plan out, and the message was answered as a question.
/// When its words ask something of that work (STEER, the same decision a message during a task
/// gets), the session is told once, as a need of its background work: it wakes to MANAGE when this
/// turn ends, with the asks no task has yet next to its decision (a message to the agent whose
/// task it is, or the work planned again). Seen: "yes, the spreadsheet files must work too" was
/// answered "I'll fold them in", reached no task, and the work was reported done without them.
/// A question that asks nothing of the work leaves nothing behind; the asks stay in the ledger
/// either way and are judged when the work ends.
async fn asks_reach_plan(env: &RunEnv, journal: &mut Journal) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    let Some(message) = entries.iter().find(|entry| entry.kind == kinds::USER_MESSAGE && str_field(&entry.data, "run_id") == env.run_id) else { return Ok(()) };
    let said = str_field(&message.data, "text").trim().to_owned();
    if said.is_empty() || of_run(&entries, &env.run_id).iter().any(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "asks_heard") {
        return Ok(());
    }
    let open_work = crate::manage::open_work(env, &entries).await;
    let Some(earlier) = earlier_at_work(env, &entries, &open_work).await else { return Ok(()) };
    let unplanned = asks_unplanned(&entries, &env.run_id, &earlier.plan.plan_id, true);
    if unplanned.is_empty() {
        return Ok(());
    }
    let asks_work = match env.data.shape("STEER").cloned() {
        Some(shape) => match decide_json::<schemas::Steer>(env, journal, "STEER", &shape, &env.data.text("steer_agents_live", &[("text", &said)])).await {
            Ok((steer, _)) => steer.kind.trim() != "question",
            Err(error) if error.code == codes::CANCELLED => return Err(error),
            Err(error) => {
                tracing::warn!(%error, "the STEER decision could not be read; the session hears the asks");
                true
            }
        },
        None => true,
    };
    // Decided once per turn, whatever it was (a resumed run does not ask again); a message that
    // asks nothing of the work is not shown to the session as an ask later ([`asks_unplanned`]).
    journal
        .append(
            kinds::NOTE,
            // `from_steer`: the message was queued while a run went on and is in the journal as that entry too.
            json!({"text": "", "purpose": "asks_heard", "run_id": env.run_id, "host_message": false, "message": message.seq, "from_steer": message.data.get("from_steer").cloned().unwrap_or(Value::Null), "asks_work": asks_work}),
        )
        .await?;
    if asks_work {
        let list = unplanned.iter().enumerate().map(|(index, ask)| format!("{}. {}", index + 1, env.data.text("ask_line", &[("quote", ask)]))).collect::<Vec<_>>().join("\n");
        journal.note(&env.run_id, "supervise", &env.data.text("supervise_asks", &[("asks", &list)]), true).await?;
    }
    Ok(())
}

/// The plan's progress for the user (a transcript note, no model call): the running task, the
/// done ones, the session's running background jobs with what their output says now (its whole
/// tail as the tools plugin keeps it). The model hears
/// that the user has it and goes on with the task.
async fn answer_progress(env: &RunEnv, journal: &mut Journal, plan: &plan::Plan, n: u32) -> Result<(), RpcError> {
    let title = plan.tasks.get(n as usize - 1).map(|task| task.title.clone()).unwrap_or_default();
    let done: Vec<String> = plan
        .steps_done
        .iter()
        .filter(|step| step.outcome == "done")
        .filter_map(|step| plan.tasks.get(step.n as usize - 1).map(|task| task.title.clone()))
        .collect();
    let done = if done.is_empty() { env.data.text("progress_done_none", &[]) } else { done.join("; ") };
    let jobs: String = match env.host.get::<cc_protocol::contracts::tools::JobListResult>(tools_methods::JOB_LIST, json!({"session_id": env.session_id})).await {
        Ok(list) => list
            .jobs
            .iter()
            .filter(|job| job.state == "running")
            .map(|job| {
                env.data.text("progress_job", &[("job_id", &job.job_id), ("tail", job.output_tail.trim())])
            })
            .collect(),
        Err(_) => String::new(),
    };
    let answer = env.data.text("progress_answer", &[("n", &n.to_string()), ("count", &plan.tasks.len().to_string()), ("title", &title), ("done", &done), ("jobs", &jobs)]);
    journal.note(&env.run_id, "progress", &answer, false).await?;
    journal.note(&env.run_id, "progress_go_on", &env.data.text("progress_go_on", &[("n", &n.to_string()), ("title", &title)]), true).await?;
    Ok(())
}

/// The message the work of plan `plan_id` started with: the user message of the run that wrote the
/// plan's first version (a plan the host started has none: the user's last message before it), or
/// of the plan whose tasks it carried on ([`plan::Plan::continues`]), whichever is earlier.
fn plan_started(entries: &[JournalEntry], plan_id: &str) -> Option<u64> {
    let mut id = plan_id.to_owned();
    let mut seen: HashSet<String> = HashSet::new();
    let mut start: Option<u64> = None;
    while !id.is_empty() && seen.insert(id.clone()) {
        let Some(first) = entries.iter().find(|entry| entry.kind == chatflow_kinds::PLAN && str_field(&entry.data, "plan_id") == id) else { break };
        let (run, before) = (str_field(&first.data, "run_id"), first.seq);
        let message = entries
            .iter()
            .find(|entry| entry.seq < before && entry.kind == kinds::USER_MESSAGE && str_field(&entry.data, "run_id") == run)
            .or_else(|| entries.iter().filter(|entry| entry.seq < before && entry.kind == kinds::USER_MESSAGE).last())
            .map(|entry| entry.seq);
        start = match (start, message) {
            (Some(known), Some(found)) => Some(known.min(found)),
            (known, found) => known.or(found),
        };
        // Every version of a plan carries what it continues; the newest says it as it stands.
        id = plan::version_of(entries, &id).and_then(|(_, plan)| plan.continues).unwrap_or_default();
    }
    start
}

/// Where the asks of the work begin (the ledger's first message): the earliest of the message that
/// set the direction ([`direction`]), the message the plan's work started with ([`plan_started`])
/// and the message that started this run; never before a message that set the earlier direction
/// aside (ROUTE's `drops`: what was asked before it no longer holds).
fn asks_start(entries: &[JournalEntry], run_id: &str, plan_id: Option<&str>) -> Option<u64> {
    let message_before = |route: &JournalEntry| entries.iter().filter(|entry| entry.seq < route.seq && entry.kind == kinds::USER_MESSAGE).last().map(|message| message.seq);
    let mut direction_seq: Option<u64> = None;
    let mut set_aside: Option<u64> = None;
    for route in entries.iter().filter(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "stage") == "route") {
        let set = route.data.get("detail").map(|detail| str_field(detail, "direction").to_owned()).unwrap_or_default();
        if set == "states" || set == "approves" {
            direction_seq = message_before(route);
        } else if route.data.get("drops").and_then(Value::as_bool) == Some(true) {
            direction_seq = None;
        }
        if route.data.get("drops").and_then(Value::as_bool) == Some(true) {
            set_aside = message_before(route);
        }
    }
    let run_seq = entries.iter().find(|entry| entry.kind == kinds::USER_MESSAGE && str_field(&entry.data, "run_id") == run_id).map(|message| message.seq);
    let plan_seq = plan_id.and_then(|id| plan_started(entries, id));
    let start = [direction_seq, plan_seq, run_seq].into_iter().flatten().min()?;
    Some(set_aside.map_or(start, |floor| start.max(floor)))
}

/// What the user asked from entry `start` on, each with the entry it was said in: each message's
/// asks as ROUTE quoted them (the whole message when it has none) and every message written during
/// the work. A question about the background agents (STATUS) and a plan card's Implement button
/// ask nothing of the work.
fn asks_since(entries: &[JournalEntry], start: u64) -> Vec<(u64, String)> {
    let mut asks: Vec<(u64, String)> = Vec::new();
    let mut add = |seq: u64, text: &str| {
        let text = text.trim();
        if !text.is_empty() && !asks.iter().any(|(_, ask)| ask == text) {
            asks.push((seq, text.to_owned()));
        }
    };
    for entry in entries.iter().filter(|entry| entry.seq >= start) {
        if entry.kind == kinds::STEER {
            add(entry.seq, str_field(&entry.data, "text"));
            continue;
        }
        if entry.kind != kinds::USER_MESSAGE || entry.data.get("implement").is_some() {
            continue;
        }
        let message_run = str_field(&entry.data, "run_id");
        let route = entries.iter().find(|route| route.kind == kinds::ROUTE && str_field(&route.data, "stage") == "route" && str_field(&route.data, "run_id") == message_run);
        let detail = route.and_then(|route| route.data.get("detail"));
        if detail.is_some_and(|detail| str_field(detail, "route") == "STATUS") {
            continue;
        }
        let quoted: Vec<schemas::Ask> = detail.and_then(|detail| detail.get("asks")).cloned().and_then(|asks| serde_json::from_value(asks).ok()).unwrap_or_default();
        let quoted: Vec<&str> = quoted.iter().map(|ask| ask.quote.trim()).filter(|quote| !quote.is_empty()).collect();
        if quoted.is_empty() {
            add(entry.seq, str_field(&entry.data, "text"));
        } else {
            quoted.into_iter().for_each(|quote| add(entry.seq, quote));
        }
    }
    asks
}

/// What the user asked of the work, in their words: the ledger the work is judged by. Every ask
/// from where the work began ([`asks_start`]) to now counts, whichever turn it was said in: the
/// request, what the user wrote while the plan was open or carried out, and what they answered in
/// between (seen: "yes, the spreadsheet files must work too", said while agents worked, was
/// answered with a promise, reached no task, and the work was judged without it).
fn work_asks(entries: &[JournalEntry], run_id: &str, plan_id: Option<&str>) -> Vec<String> {
    let Some(start) = asks_start(entries, run_id, plan_id) else { return Vec::new() };
    asks_since(entries, start).into_iter().map(|(_, ask)| ask).collect()
}

/// The asks of the work no plan has taken up yet: said after the newest version of plan `plan_id`
/// was written. `unheard`: only those the session has not heard at a MANAGE decision yet (told
/// once: an ask the session answered with a message to an agent is not shown to it again at every
/// check); a plan written to take them up gets them all.
pub(crate) fn asks_unplanned(entries: &[JournalEntry], run_id: &str, plan_id: &str, unheard: bool) -> Vec<String> {
    let Some(start) = asks_start(entries, run_id, Some(plan_id)) else { return Vec::new() };
    let planned = plan::version_of(entries, plan_id).map(|(seq, _)| seq).unwrap_or(0);
    let told = if unheard {
        entries
            .iter()
            .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "managed" && entry.data.get("asks_told").and_then(Value::as_bool) == Some(true))
            .map(|entry| entry.seq)
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    // A message STEER read as only a question ([`asks_reach_plan`]) asks nothing a task could take up.
    let questions: HashSet<u64> = entries
        .iter()
        .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "asks_heard" && entry.data.get("asks_work").and_then(Value::as_bool) == Some(false))
        .flat_map(|entry| ["message", "from_steer"].into_iter().filter_map(move |field| entry.data.get(field).and_then(Value::as_u64)))
        .collect();
    asks_since(entries, start).into_iter().filter(|(seq, _)| *seq > planned && *seq > told && !questions.contains(seq)).map(|(_, ask)| ask).collect()
}

/// What the work's runs printed, for ACCEPT: the newest result of each command this run's `run`
/// calls ran (not the plan's checks, not test runs, not a background start), oldest first, and the
/// background jobs' results. The newest eight: the ones the work ended with.
fn project_runs(data: &crate::data::Dataset, entries: &[JournalEntry], run_id: &str, checks: &HashSet<String>) -> Vec<String> {
    let Some(first) = entries.iter().find(|entry| str_field(&entry.data, "run_id") == run_id).map(|entry| entry.seq) else { return Vec::new() };
    let results: HashMap<&str, &JournalEntry> =
        entries.iter().filter(|entry| entry.kind == kinds::TOOL_RESULT && str_field(&entry.data, "run_id") == run_id).map(|entry| (str_field(&entry.data, "call_id"), entry)).collect();
    // The files this run created or changed: a run of one of them is the work's own script.
    let mut changed: Vec<String> = results
        .values()
        .flat_map(|result| result.data.get("changed_files").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned))
        .collect();
    changed.sort();
    changed.dedup();
    let mut runs: Vec<(String, String)> = Vec::new();
    for entry in entries.iter().filter(|entry| entry.seq >= first) {
        if entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "job" {
            let text = str_field(&entry.data, "text").trim();
            if !text.is_empty() {
                runs.push((format!("job:{}", entry.seq), data.text("accept_job", &[("text", text)])));
            }
            continue;
        }
        if entry.kind != kinds::TOOL_CALL || str_field(&entry.data, "run_id") != run_id || str_field(&entry.data, "tool") != "run" {
            continue;
        }
        let command = entry.data.get("args").map(|args| str_field(args, "command").trim().to_owned()).unwrap_or_default();
        // A check run by hand, piped or after a `cd` (`cargo test 2>&1 | tail -30`), is still the
        // check: its passing test lines say the tests pass, not what the software does (seen: asks
        // about the software's results judged met on `test ... ok` lines).
        if command.is_empty() || checks.contains(&command) || runs_a_check(&command, checks) {
            continue;
        }
        let Some(result) = results.get(str_field(&entry.data, "call_id")) else { continue };
        let display = result.data.get("display").cloned().unwrap_or(Value::Null);
        if display.get("tests").is_some_and(|tests| !tests.is_null()) || display.get("background").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        // A hidden status (`exit_hidden`: the status was another command's) is said as not shown,
        // so the run is never read as a pass.
        let exit = match display.get("exit_code").and_then(Value::as_i64) {
            Some(code) => code.to_string(),
            None if display.get("exit_hidden").and_then(Value::as_bool) == Some(true) => data.text("accept_exit_hidden", &[]),
            None => "-".to_owned(),
        };
        let output = str_field(&result.data, "output").trim_end();
        runs.retain(|(key, _)| key != &command);
        runs.push((command.clone(), accept_run_text(data, &command, &exit, output, &changed)));
    }
    let skip = runs.len().saturating_sub(8);
    runs.into_iter().skip(skip).map(|(_, text)| text).collect()
}

/// Whether a command runs a script the work itself created or changed: it names one of `files`
/// by its path or file name, or a Rust example or binary by its name (`--example e2erun` for
/// `examples/e2erun.rs`). What such a run prints is the work's own account, not the software
/// shown working (seen: a driver the work wrote printing "first run: OK", judged as every ask met).
fn runs_own_script(command: &str, files: &[String]) -> bool {
    files.iter().any(|file| {
        let path = std::path::Path::new(file);
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
        let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("");
        let in_examples = file.contains("examples/") || file.contains("src/bin/");
        command.contains(file.as_str())
            || (name.len() > 3 && name.contains('.') && command.contains(name))
            || (in_examples && stem.len() > 2 && (command.contains(&format!("--example {stem}")) || command.contains(&format!("--bin {stem}"))))
    })
}

/// One run as ACCEPT reads it, marked when it runs a script the work wrote ([`runs_own_script`]).
fn accept_run_text(data: &crate::data::Dataset, command: &str, exit: &str, output: &str, files: &[String]) -> String {
    let key = if runs_own_script(command, files) { "accept_run_own" } else { "accept_run" };
    data.text(key, &[("command", command), ("exit", exit), ("output", output)])
}

/// Whether one of the shell command's parts (split at `&&`, `||`, `;` and `|`) starts with one
/// of `checks`.
fn runs_a_check(command: &str, checks: &HashSet<String>) -> bool {
    command
        .split(['|', ';', '&'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .any(|part| checks.iter().any(|check| !check.is_empty() && (part == check || part.starts_with(&format!("{check} ")))))
}

/// The host's checks as the newest verification of this run left them, one line each (`cargo
/// test` passes / fails); empty when the run verified nothing.
fn host_checks(data: &crate::data::Dataset, entries: &[JournalEntry], run_id: &str) -> String {
    let Some(first) = entries.iter().find(|entry| str_field(&entry.data, "run_id") == run_id).map(|entry| entry.seq) else { return String::new() };
    let Some(report) = entries.iter().rev().filter(|entry| entry.seq >= first && entry.kind == kinds::VERIFICATION).find_map(|entry| entry.data.get("report")) else {
        return String::new();
    };
    let specs: HashMap<&str, &str> = report
        .get("contract")
        .and_then(|contract| contract.get("checks"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|check| (str_field(check, "name"), str_field(check, "spec")))
        .collect();
    report
        .get("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|check| {
            let name = str_field(check, "name");
            let command = specs.get(name).copied().unwrap_or(name);
            let key = if check.get("ok").and_then(Value::as_bool) == Some(true) { "accept_check_passes" } else { "accept_check_fails" };
            data.text(key, &[("command", command)])
        })
        .collect()
}

/// What ACCEPT is shown of the work, each part worded for its place in `accept_live`.
struct Shown {
    files: String,
    checks: String,
    runs: String,
}

/// ACCEPT: each of `asks` judged by what is shown of the work. Some(what is not met yet, one line
/// per ask) when an ask is not shown met; None when every ask is, or when the decision cannot be
/// read (the work is reported as it is). The verdict on every ask is kept on an `accept` note with
/// `mark`'s fields (what was judged: a run's own work, or a finished tree of agents' tasks); a
/// decision that could not be read leaves the note without a verdict, so the same work is not
/// judged twice.
async fn judge_asks(env: &RunEnv, journal: &mut Journal, asks: &[String], shown: Shown, mark: Value) -> Option<String> {
    let shape = env.data.shape("ACCEPT").cloned()?;
    let numbers: Vec<String> = (1..=asks.len()).map(|number| number.to_string()).collect();
    let shape = shape.filled(schemas::ASKS_PLACEHOLDER, &numbers, None);
    let list: String = asks.iter().enumerate().map(|(index, ask)| format!("{}. {}\n", index + 1, env.data.text("ask_line", &[("quote", ask)]))).collect();
    let live = env.data.text("accept_live", &[("asks", list.trim_end()), ("files", &shown.files), ("checks", shown.checks.trim_end()), ("runs", shown.runs.trim_end())]);
    let marked = |note: Value| {
        let mut note = note;
        if let (Some(into), Some(from)) = (note.as_object_mut(), mark.as_object()) {
            for (key, value) in from {
                into.insert(key.clone(), value.clone());
            }
        }
        note
    };
    match decide_json::<schemas::Accept>(env, journal, "ACCEPT", &shape, &live).await {
        Ok((accept, _)) => {
            let unmet: String = accept
                .asks
                .iter()
                .filter(|judged| matches!(judged.met.trim(), "no" | "unshown") && !judged.missing.trim().is_empty())
                .filter_map(|judged| {
                    let quote = asks.get(judged.ask.trim().parse::<usize>().ok()?.checked_sub(1)?)?;
                    let evidence = judged.evidence.trim();
                    let evidence = if evidence.is_empty() { String::new() } else { env.data.text("accept_unmet_evidence", &[("evidence", evidence)]) };
                    Some(env.data.text("accept_unmet_item", &[("quote", quote), ("missing", judged.missing.trim()), ("evidence", &evidence)]))
                })
                .collect();
            // The verdict on every ask is kept with the note: the report of the work tells each ask
            // as it was judged ([`work_outcome`]), whether or not the work goes on for it.
            let verdict: Vec<Value> = accept
                .asks
                .iter()
                .filter_map(|judged| {
                    let quote = asks.get(judged.ask.trim().parse::<usize>().ok()?.checked_sub(1)?)?;
                    Some(json!({"quote": quote, "met": judged.met.trim(), "missing": judged.missing.trim(), "evidence": judged.evidence.trim()}))
                })
                .collect();
            let text = if unmet.is_empty() { env.data.text("accept_met", &[]) } else { env.data.text("accept_note", &[("unmet", unmet.trim_end())]) };
            let note = marked(json!({"text": text, "purpose": "accept", "run_id": env.run_id, "host_message": false, "asks": verdict}));
            if let Err(error) = journal.append(kinds::NOTE, note).await {
                tracing::warn!(%error, "the ACCEPT verdict could not be recorded");
            }
            if unmet.is_empty() {
                return None;
            }
            Some(unmet.trim_end().to_owned())
        }
        Err(error) => {
            tracing::warn!(%error, "the ACCEPT decision could not be read; the work is reported as it is");
            let note = marked(json!({"text": "", "purpose": "accept", "run_id": env.run_id, "host_message": false}));
            let _ = journal.append(kinds::NOTE, note).await;
            None
        }
    }
}

/// ACCEPT when every task of the plan at `plan_seq` is done: each ask of the user judged by what
/// the work's runs printed ([`judge_asks`]). None also when there is nothing to judge.
async fn accept_plan(env: &RunEnv, journal: &mut Journal, plan_seq: u64) -> Option<String> {
    let entries = journal.entries_vec();
    let plan = plan::at(&entries, plan_seq)?;
    let asks = work_asks(&entries, &env.run_id, Some(&plan.plan_id));
    if asks.is_empty() {
        return None;
    }
    let checks: HashSet<String> = plan.tasks.iter().flat_map(|task| task.checks.iter().map(|check| check.trim().to_owned())).collect();
    let runs = project_runs(&env.data, &entries, &env.run_id, &checks);
    let runs = if runs.is_empty() { env.data.text("accept_no_runs", &[]) } else { runs.join("\n") };
    let mut files: Vec<String> = plan.steps_done.iter().flat_map(|step| step.changed_files.iter().cloned()).collect();
    files.sort();
    files.dedup();
    let files = if files.is_empty() { "-".to_owned() } else { files.join(", ") };
    let checks = host_checks(&env.data, &entries, &env.run_id);
    let checks = if checks.is_empty() { env.data.text("accept_no_checks", &[]) } else { checks };
    // What the reviews between tasks found stays in view of the verdict: a gap a review named and
    // the plan did not take up is the work's own account of what it left (seen: "CLIP search cannot
    // run in this repo ... a real gap the remaining tasks don't address", then the ask judged met).
    let found = reviews_found(&entries, &env.run_id);
    let runs = if found.is_empty() { runs } else { format!("{runs}\n\n{}", env.data.text("accept_found", &[("found", found.trim_end())])) };
    judge_asks(env, journal, &asks, Shown { files, checks, runs }, json!({"plan_seq": plan_seq})).await
}

/// What this run's REVIEW decisions found, one line each, oldest first (empty when none found
/// anything).
fn reviews_found(entries: &[JournalEntry], run_id: &str) -> String {
    entries
        .iter()
        .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "stage") == "REVIEW" && str_field(&entry.data, "purpose") == "decision_text")
        .filter_map(|entry| {
            let raw = entry.data.get("raw").and_then(Value::as_str)?;
            let decision: Value = serde_json::from_str(raw.trim()).ok()?;
            let found = decision.get("found")?.as_str()?.trim();
            (!found.is_empty()).then(|| format!("- {found}\n"))
        })
        .collect()
}

/// The newest finished tree of agents' tasks nobody has judged yet (the agents plugin's
/// `delegation_done` note with what each task left: its state, files and last runs), with the plan
/// version it carried out (the one whose root task it names). None when there is none, when it was
/// judged already (an `accept` note names it), or when no plan of this conversation made the tree
/// (work handed over without a plan has no asks ledger to judge by).
fn unjudged_tree(entries: &[JournalEntry]) -> Option<(u64, Vec<Value>, u64, plan::Plan)> {
    let done = entries.iter().rev().find(|entry| {
        entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "delegation_done" && entry.data.get("tasks").and_then(Value::as_array).is_some_and(|tasks| !tasks.is_empty())
    })?;
    let judged = entries.iter().any(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "accept" && entry.data.get("delegation").and_then(Value::as_u64) == Some(done.seq));
    if judged {
        return None;
    }
    let root = str_field(&done.data, "task_id");
    let (plan_seq, plan) = entries.iter().rev().find_map(|entry| plan::parse(entry).filter(|plan| !root.is_empty() && plan.root_task.as_deref() == Some(root)).map(|plan| (entry.seq, plan)))?;
    let facts = done.data.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
    Some((done.seq, facts, plan_seq, plan))
}

/// A plan the agents carried out is done as a tree of tasks; whether it is right is judged here,
/// against what the user asked, before the session reports it (seen: nine agent tasks, none with a
/// check, reported "done and green" with almost none of the asked work working; acceptance ran
/// only for plans the session carried out itself).
/// 1. Each task's end is recorded on the plan, from the agents plugin's facts: the report of the
///    work says each task's state from these ([`tree_outcome`]).
/// 2. ACCEPT judges every ask of the work by the tasks' states, the files they changed and what
///    their agents' runs of the project's own commands printed last.
/// 3. An ask not shown met, within `limits.accepts`: the tasks that meet it are written (the unmet
///    replan) and go to agents as the plan did. True then: the work goes on, and the `managed` note
///    says so.
/// False when there is no such tree, when every ask is met, or when the acceptance rounds are
/// spent: the reply is written from what was recorded.
pub(crate) async fn accept_tree(env: &RunEnv, journal: &mut Journal) -> Result<bool, RpcError> {
    let entries = journal.entries_vec();
    let Some((delegation, facts, plan_seq, plan)) = unjudged_tree(&entries) else { return Ok(false) };
    let fact_of = |task: &plan::PlanTask| facts.iter().find(|fact| task.task_id.as_deref().is_some_and(|id| str_field(fact, "task_id") == id));
    let list = |fact: &Value, field: &str| -> Vec<String> { fact.get(field).and_then(Value::as_array).into_iter().flatten().filter_map(|item| item.as_str().map(str::to_owned)).collect() };
    // Every task of a finished tree is done; what checked it is its state.
    let steps: Vec<plan::StepDone> = plan
        .tasks
        .iter()
        .map(|task| {
            let fact = fact_of(task);
            let (check, detail) = match fact.map(|fact| str_field(fact, "state")).unwrap_or("") {
                "passed" => ("passed", String::new()),
                "failed" => ("left", task.checks.join("`, `")),
                _ => ("", String::new()),
            };
            plan::StepDone { n: task.n, outcome: "done".into(), summary: String::new(), changed_files: fact.map(|fact| list(fact, "changed_files")).unwrap_or_default(), check: check.into(), check_detail: detail }
        })
        .collect();
    journal.set(plan_seq, "steps_done", serde_json::to_value(&steps)?).await?;
    let asks = work_asks(&entries, &env.run_id, Some(&plan.plan_id));
    let mark = json!({"delegation": delegation, "plan_seq": plan_seq});
    if asks.is_empty() {
        // Nothing to judge by: the tree is marked seen, and the reply tells each task's state.
        journal.append(kinds::NOTE, json!({"text": "", "purpose": "accept", "run_id": env.run_id, "host_message": false, "delegation": delegation, "plan_seq": plan_seq})).await?;
        return Ok(false);
    }
    let mut changed: Vec<String> = facts.iter().flat_map(|fact| list(fact, "changed_files")).collect();
    changed.sort();
    changed.dedup();
    let files = if changed.is_empty() { "-".to_owned() } else { changed.join(", ") };
    let checks: String = plan
        .tasks
        .iter()
        .map(|task| {
            let state = match fact_of(task).map(|fact| str_field(fact, "state")).unwrap_or("") {
                "passed" => "accept_task_passed",
                "failed" => "accept_task_failing",
                _ => "accept_task_unchecked",
            };
            env.data.text("accept_task", &[("title", &task.title), ("state", &env.data.text(state, &[]))])
        })
        .collect();
    let runs: String = facts
        .iter()
        .flat_map(|fact| fact.get("runs").and_then(Value::as_array).cloned().unwrap_or_default())
        .map(|run| {
            let exit = run.get("exit_code").and_then(Value::as_i64).map(|code| code.to_string()).unwrap_or_else(|| "-".to_owned());
            accept_run_text(&env.data, str_field(&run, "command"), &exit, str_field(&run, "output").trim_end(), &changed)
        })
        .collect();
    let runs = if runs.is_empty() { env.data.text("accept_no_runs", &[]) } else { runs };
    let Some(unmet) = judge_asks(env, journal, &asks, Shown { files, checks, runs }, mark).await else { return Ok(false) };
    if plan.accepts >= env.data.limits.accepts {
        return Ok(false);
    }
    let failed = plan.tasks.len().max(1) as u32;
    if !write_plan(env, journal, PlanMode::Replan { plan_seq, failed, outcome: "unmet".into(), summary: unmet.clone() }).await? {
        return Ok(false);
    }
    let Some((new_seq, mut made)) = plan::for_run(&journal.entries_vec(), &env.run_id) else { return Ok(false) };
    // A rewrite that added no task leaves nothing to start: the work is reported as it stands,
    // with the asks as they were judged.
    if made.tasks.len() <= made.steps_done.len() {
        return Ok(false);
    }
    // The tree goes on: its root is open again while the new tasks are worked on.
    if let Some(root) = made.root_task.clone()
        && let Err(error) = env.host.get::<Task>(memory_methods::TASK_UPDATE, json!({"id": root, "status": "in_progress"})).await
    {
        tracing::warn!(%error, task = %root, "the plan's root task could not be opened again");
    }
    let started = match implement_by_agents(env, journal, new_seq, &mut made, false).await {
        Ok(text) => text,
        Err(error) if error.code == codes::CANCELLED => return Err(error),
        Err(error) => env.data.text("implement_failed", &[("error", &error.message)]),
    };
    let text = joined(&[env.data.text("accept_note", &[("unmet", &unmet)]), started]);
    journal.note(&env.run_id, "managed", &text, true).await?;
    Ok(true)
}

fn decide_routes(full: Shape, route: &str) -> Shape {
    match route {
        // A plan request ends in a plan: what is still unknown becomes a step of it, not a
        // question back (seen: "plan" read the model folder in the wrong place, then asked the
        // user where it was instead of planning).
        "PLAN" => full.without("route", "DO").without("route", "DELEGATE").without("route", "ANSWER"),
        "DELEGATE" => full.without("route", "DO").without("route", "PLAN").without("route", "ANSWER"),
        // A change keeps ANSWER: a request that can be read two ways is asked back to the user.
        _ => full,
    }
}

/// One task's (or the whole work's) state as the host recorded it, in plain words: how its loop
/// ended and what the host's checks showed then.
pub(crate) fn state_text(data: &crate::data::Dataset, outcome: &str, check: &str, detail: &str, summary: &str) -> String {
    if outcome != "done" {
        // The first paragraph of what stopped it: the report names the cause, not the whole log.
        let why = summary.trim().split("\n\n").next().unwrap_or("").trim();
        return data.text("outcome_state_ended", &[("outcome", outcome), ("summary", why)]);
    }
    match check {
        "passed" => data.text("outcome_state_checked", &[]),
        "left" => data.text("outcome_state_left", &[("checks", detail)]),
        "unrun" => data.text("outcome_state_unrun", &[("checks", detail)]),
        "failed" => data.text("outcome_state_failing", &[("checks", detail)]),
        _ => data.text("outcome_state_unchecked", &[]),
    }
}

/// Each task of `plan` with how it ended and what the checks showed on it, one line each (a task
/// no loop was started for is "never started": the run ended, or its replans were spent, before it).
fn tasks_outcome(data: &crate::data::Dataset, plan: &plan::Plan) -> String {
    plan.tasks
        .iter()
        .map(|task| {
            let n = task.n.to_string();
            match plan.steps_done.iter().rev().find(|step| step.n == task.n) {
                Some(step) => {
                    let state = state_text(data, &step.outcome, &step.check, &step.check_detail, &step.summary);
                    data.text("outcome_task", &[("n", &n), ("title", &task.title), ("state", &state)])
                }
                None => data.text("outcome_task_unstarted", &[("n", &n), ("title", &task.title)]),
            }
        })
        .collect()
}

/// Each ask as an `accept` note's verdict judged it, one line each, under their heading; empty
/// when the note holds no verdict.
fn asks_outcome(data: &crate::data::Dataset, accept: &JournalEntry) -> String {
    let asks: String = accept
        .data
        .get("asks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|ask| {
            let quote = str_field(ask, "quote");
            match (str_field(ask, "met"), str_field(ask, "missing")) {
                ("yes", _) => data.text("outcome_ask_met", &[("quote", quote)]),
                (_, "") => data.text("outcome_ask_unshown", &[("quote", quote)]),
                (_, missing) => data.text("outcome_ask_unmet", &[("quote", quote), ("missing", missing)]),
            }
        })
        .collect();
    if asks.is_empty() { String::new() } else { data.text("outcome_asks", &[("asks", &asks)]) }
}

/// What the host recorded of the work this run did, for the reply: each task of its plan with how
/// it ended and what the checks showed on it ([`tasks_outcome`]), the checks as the newest
/// verification left them, and each ask as ACCEPT judged it when that verdict is of the work as it
/// ended. Empty for a run that did no work.
fn work_outcome(data: &crate::data::Dataset, entries: &[JournalEntry], run_id: &str) -> String {
    let Some(act) = entries.iter().rev().find(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == "act_done") else {
        return String::new();
    };
    let planned = plan::for_run(entries, run_id);
    let tasks: String = match &planned {
        Some((_, plan)) if !plan.tasks.is_empty() => tasks_outcome(data, plan),
        // A change made without a plan: the one loop's own end.
        _ => {
            let state = state_text(data, str_field(&act.data, "outcome"), str_field(&act.data, "check"), str_field(&act.data, "check_detail"), str_field(&act.data, "summary"));
            data.text("outcome_work", &[("state", &state)])
        }
    };
    let checks = host_checks(data, entries, run_id);
    let checks = if checks.is_empty() { String::new() } else { data.text("outcome_checks", &[("checks", &checks)]) };
    // ACCEPT's verdict counts when no plan version was written after it (a later version means
    // more work was done since, which that verdict did not see).
    let newest_plan = planned.as_ref().map(|(seq, _)| *seq).unwrap_or(0);
    let asks = entries
        .iter()
        .rev()
        .find(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == "accept" && entry.data.get("asks").is_some())
        .filter(|entry| entry.seq > newest_plan)
        .map(|entry| asks_outcome(data, entry))
        .unwrap_or_default();
    data.text("reply_outcome", &[("tasks", &tasks), ("checks", &checks), ("asks", &asks)])
}

/// What the host recorded of a plan the agents carried out, for the reply of the run that judged
/// its finished tree ([`accept_tree`]): each task as the agents plugin left it and each ask as
/// ACCEPT judged it. The verdict is left out when the work goes on (a plan version written after
/// it with tasks still to do: it is no longer the state of the work). Empty for a run that judged
/// no tree.
fn tree_outcome(data: &crate::data::Dataset, entries: &[JournalEntry], run_id: &str) -> String {
    let Some(accept) = entries
        .iter()
        .rev()
        .find(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == "accept" && entry.data.get("delegation").is_some())
    else {
        return String::new();
    };
    let Some(plan) = accept.data.get("plan_seq").and_then(Value::as_u64).and_then(|seq| plan::at(entries, seq)) else { return String::new() };
    let goes_on = plan::version_of(entries, &plan.plan_id).is_some_and(|(seq, newest)| seq > accept.seq && newest.tasks.len() > newest.steps_done.len());
    let asks = if goes_on { String::new() } else { asks_outcome(data, accept) };
    data.text("reply_outcome", &[("tasks", &tasks_outcome(data, &plan)), ("checks", ""), ("asks", &asks)])
}

fn act_note(outcome: &LoopOutcome) -> String {
    let verified = match (outcome.verified, outcome.check.as_str()) {
        (Some(true), _) => " The host's checks pass.",
        (Some(false), _) => " The host's checks do not pass.",
        (None, "unrun") => " The host's checks could not run here.",
        (None, _) => "",
    };
    let files = if outcome.changed_files.is_empty() { "no files".to_owned() } else { outcome.changed_files.join(", ") };
    let calls = if outcome.tool_calls == 1 { "1 tool call".to_owned() } else { format!("{} tool calls", outcome.tool_calls) };
    format!("Work {}: {calls}, changed {files}.{verified}", outcome.outcome)
}

/// The tasks this run's plan rewrites left out, with why: the report of the work says what was
/// not done, so "done" never hides a dropped part of the request.
fn dropped_note(data: &crate::data::Dataset, entries: &[JournalEntry], run_id: &str) -> String {
    let dropped = dropped_tasks(entries, run_id);
    if dropped.is_empty() {
        return String::new();
    }
    data.text("plan_dropped", &[("tasks", &dropped.join("; "))])
}

/// The tasks this run's plan rewrites left out, each "title: why".
fn dropped_tasks(entries: &[JournalEntry], run_id: &str) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| entry.kind == chatflow_kinds::PLAN && str_field(&entry.data, "run_id") == run_id)
        .filter_map(plan::parse)
        .filter_map(|plan| plan.diff)
        .flat_map(|diff| diff.tasks_dropped)
        .collect()
}

fn delegated_note(data: &crate::data::Dataset, result: &DelegateResult) -> String {
    let mut text = data.text("delegated", &[("title", &result.root_task.title), ("count", &result.tasks.len().to_string())]);
    for task in &result.tasks {
        let assignee = task.assignee.as_deref().map(|name| data.text("delegated_assignee", &[("name", name)])).unwrap_or_default();
        let depends = if task.depends_on.is_empty() { String::new() } else { data.text("delegated_after", &[("count", &task.depends_on.len().to_string())]) };
        text.push_str(&data.text("delegated_task", &[("title", &task.title), ("assignee", &assignee), ("depends", &depends)]));
    }
    let working: Vec<&str> = result.agents.iter().map(|agent| agent.name.as_str()).collect();
    if !working.is_empty() {
        text.push_str(&data.text("delegated_agents", &[("names", &working.join(", "))]));
    }
    text
}

/// Join the non-empty parts of a stage message's live text.
fn joined(parts: &[String]) -> String {
    parts.iter().map(|part| part.trim()).filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n")
}

/// How a plan is written: asked for by the user (PLAN, or PLAN_EDIT of the open plan), made for
/// a DO decision (`Auto`: carried out at once), or made again after one of its tasks ended badly
/// (`Replan`: the done tasks stay, the rest is written anew).
enum PlanMode {
    Asked,
    Auto,
    Replan { plan_seq: u64, failed: u32, outcome: String, summary: String },
}

async fn run_plan(env: &RunEnv, journal: &mut Journal, auto: bool) -> Result<(), RpcError> {
    write_plan(env, journal, if auto { PlanMode::Auto } else { PlanMode::Asked }).await.map(|_| ())
}

/// PLAN (a new plan) or PLAN_EDIT (a plan's next version, with its difference). Nothing is
/// changed in the repository. An auto plan and a replan are recorded as implementing with the run
/// as `implement_run` (the Act stage carries their tasks out; they are never the open plan a
/// later message edits).
/// False when nothing was written: a runner-mode session's replan that the manager refused
/// ([`crate::runner::hold`]); the plan stays as it was.
async fn write_plan(env: &RunEnv, journal: &mut Journal, mode: PlanMode) -> Result<bool, RpcError> {
    // ROUTE's skills are in the conversation already and serve the planner only; each task chooses
    // the skills of its own work.
    let entries = journal.entries_vec();
    let auto = !matches!(mode, PlanMode::Asked);
    let open = if auto { None } else { plan::open(&entries) };
    let previous = match &mode {
        PlanMode::Replan { plan_seq, .. } => plan::at(&entries, *plan_seq),
        _ => open.as_ref().map(|(_, open)| open.clone()),
    };
    // PLAN_EDIT is worded for a user's change to the open plan; a replan writes a fresh plan with
    // the replan text (what ended badly, which tasks stay).
    let stage = if matches!(mode, PlanMode::Asked) && previous.is_some() { "PLAN_EDIT" } else { "PLAN" };
    let max = env.data.limits.plan_max_tasks;
    let previous_skills = skillsel::previous_turn(&entries, &env.run_id);
    let catalog = skillsel::ordered_catalog(&env.prefix.skills, &previous_skills);
    let skills_max = env.data.limits.skills_per_turn;
    // Each task's `checks` is a pick of the project's own check commands (none: no checks).
    let candidates = crate::verifiers::candidates(env, &run_files(&entries, &env.run_id)).await;
    // The session's background work still open: the plan may take its place (`replaces`).
    let open_work = crate::manage::open_work(env, &entries).await;
    let open_labels: Vec<String> = open_work.iter().map(crate::manage::OpenWork::label).collect();
    // One plan is at work in a folder at a time, and the host says which: a plan carried out at
    // once takes the place of an earlier plan of this conversation whose agents still work. That
    // plan's tasks still to do are carried like a replan's (named in `from`, dropped with why, or
    // put back), its work stops when this plan starts, and what the user asked of it is asked of
    // this plan too (seen: a second plan started beside the first; two agents did the same task
    // and one deleted the other's files).
    let earlier = if matches!(mode, PlanMode::Auto) { earlier_at_work(env, &entries, &open_work).await } else { None };
    // A plan written again names, for each task still to do from before, the new task that carries
    // it on or why it is dropped (seen: "run the end-to-end test" left out of a rewrite after task
    // 1, never run, the work reported done).
    let pending: Vec<plan::PlanTask> = match (&mode, &previous) {
        (PlanMode::Replan { .. }, Some(previous)) => {
            let done = previous.steps_done.iter().filter(|step| step.outcome == "done").count();
            // The host's own last task ("Make every check pass") is not the planner's to carry:
            // it is made again from the checks of the tasks written now ([`final_check_task`]).
            // Carried, it brought back checks the user had said to drop (seen: `cargo test` back
            // after "I don't care about unit tests", and the run spent itself on a test again).
            let fix_title = env.data.text("baseline_task_title", &[]);
            previous.tasks.iter().skip(done).filter(|task| task.title != fix_title).cloned().collect()
        }
        _ => earlier.as_ref().map(|earlier| earlier.pending.clone()).unwrap_or_default(),
    };
    let pending_titles: Vec<String> = pending.iter().map(|task| task.title.clone()).collect();
    let shape = stage_shape(env, stage)?
        .filled(schemas::OPEN_WORK_PLACEHOLDER, &open_labels, None)
        .filled(schemas::PENDING_PLACEHOLDER, &pending_titles, None)
        .filled(schemas::TASKS_PLACEHOLDER, &plan::task_numbers(max), None)
        .filled(schemas::VERIFIERS_PLACEHOLDER, &crate::verifiers::commands(&candidates), None)
        .with_catalog(&catalog, skills_max);
    // The planner is told how its plan is used: carried out at once task by task (auto), written
    // again after a task ended badly, or the next version of the open plan.
    // An auto plan whose tasks agents can take runs its independent tasks as agents at once
    // (`parallel_implementation`), so the planner is told that instead of "here, one at a time".
    // Whether agents can take them is structural ([`agents_can_take`]), never how many are free
    // this moment: the work this plan replaces still holds its places while it is written.
    let parallel = agents_can_take(env).await;
    let edit = match &mode {
        // A lead session's plan is cut for frontier agents (`plan_lead`, below), not for the local one.
        PlanMode::Auto if parallel && !env.lead => env.data.text("plan_auto_parallel_live", &[]),
        PlanMode::Auto => env.data.text("plan_auto_live", &[]),
        PlanMode::Replan { failed, outcome, summary, .. } => {
            let plan = previous.clone().unwrap_or_default();
            let done = plan.steps_done.iter().filter(|step| step.outcome == "done").count();
            let title = plan.tasks.get(*failed as usize - 1).map(|task| task.title.clone()).unwrap_or_default();
            // A task that ended well but showed the rest must change (REVIEW), one the user wrote
            // during (a steer), or one that ended badly.
            let key = match outcome.as_str() {
                "done" => "plan_revise_live",
                "steered" => "plan_steer_live",
                "unmet" => "plan_unmet_live",
                _ => "plan_replan_live",
            };
            // What each finished task reported, in its own words: the plan written again builds on
            // what the work established (seen: a research task found the model in the project's own
            // library; two hours later a replan dropped the part as "not compatible", from memory).
            let reports: String = plan
                .steps_done
                .iter()
                .filter(|step| step.outcome == "done" && !step.summary.trim().is_empty())
                .map(|step| {
                    let title = plan.tasks.get(step.n as usize - 1).map(|task| task.title.as_str()).unwrap_or("");
                    let summary: String = step.summary.split_whitespace().collect::<Vec<_>>().join(" ");
                    env.data.text("plan_done_report", &[("n", &step.n.to_string()), ("title", title), ("summary", &summary)])
                })
                .collect();
            let reports = if reports.is_empty() { String::new() } else { env.data.text("plan_done_reports", &[("reports", reports.trim_end())]) };
            joined(&[
                env.data.text("plan_auto_live", &[]),
                reports,
                env.data.text(key, &[("n", &failed.to_string()), ("title", &title), ("outcome", outcome), ("summary", summary.trim()), ("done", &done.to_string())]),
            ])
        }
        PlanMode::Asked => open
            .as_ref()
            .map(|(_, open)| env.data.text("plan_live_edit", &[("plan_id", &open.plan_id), ("version", &open.version.to_string())]))
            .unwrap_or_default(),
    };
    // A lead session's tasks go to frontier agents, which plan their own steps: the cut is by whole
    // pieces of the work (seen: the local cut's "Lay out …" task, an empty file, as a whole agent run).
    let parallel = if env.lead {
        env.data.text("plan_lead", &[])
    } else if parallel && !auto {
        env.data.text("plan_parallel", &[])
    } else {
        String::new()
    };
    // The work earlier plans of this conversation finished: the new plan builds on it (a replan
    // keeps its own done tasks itself).
    let done = if matches!(mode, PlanMode::Replan { .. }) { Vec::new() } else { done_earlier(env, &entries).await };
    let done_live = if done.is_empty() {
        String::new()
    } else {
        let items: String = done
            .iter()
            .map(|(title, files)| env.data.text("plan_done_item", &[("title", title), ("files", &if files.is_empty() { "-".to_owned() } else { files.join(", ") })]))
            .collect();
        env.data.text("plan_done_live", &[("items", &items)])
    };
    // Each task's `skills` choose from the catalog, shown last.
    // The earlier plan's own work is told with its carried tasks below (it stops); what is listed
    // here is the work that may go on next to this plan.
    let beside: Vec<crate::manage::OpenWork> = open_work.iter().filter(|item| earlier.as_ref().is_none_or(|earlier| !earlier.work.contains(&item.task.id))).cloned().collect();
    let open_live = if beside.is_empty() { String::new() } else { env.data.text("open_work_live", &[("work", &crate::manage::work_lines(env, &beside))]) };
    // The direction the user set, then their latest words as they wrote them, right before how
    // the plan is written: the project's rules and the earlier work are in every prompt, and the
    // planner wrote the old design again from them (seen: twice after the user rejected it).
    let direction_live = direction(&env.data, &entries);
    let mut asks = asks_live(&env.data, &entries, &env.run_id, "plan_asks", false);
    // A plan the host starts (the session's replan of its agents' work) has no message of its own:
    // it is shown what the user asked of the work that no plan took up yet.
    if asks.is_empty()
        && let Some(earlier) = &earlier
    {
        let unplanned = asks_unplanned(&entries, &env.run_id, &earlier.plan.plan_id, false);
        if !unplanned.is_empty() {
            let list = unplanned.iter().enumerate().map(|(index, ask)| format!("{}. {}", index + 1, env.data.text("ask_line", &[("quote", ask)]))).collect::<Vec<_>>().join("\n");
            asks = env.data.text("plan_asks", &[("asks", &list)]);
        }
    }
    let carry_live = match &earlier {
        Some(earlier) if !pending.is_empty() => {
            let titles: String = pending.iter().map(|task| env.data.text("list_item", &[("text", &task.title)])).collect::<Vec<_>>().join("\n");
            env.data.text("plan_carry_live", &[("plan_id", &earlier.plan.plan_id), ("tasks", &titles)])
        }
        _ => String::new(),
    };
    // The planner thinks before it writes: a constrained call cannot (its grammar takes the whole
    // generation), so the thinking is a free call of its own, and its notes are the newest thing in
    // the conversation when the plan is written. For a new plan, and for one written again after a
    // task ended badly; a review, a steer and an unmet ask each say themselves what must change.
    let think = match &mode {
        PlanMode::Auto => true,
        PlanMode::Asked => previous.is_none(),
        PlanMode::Replan { outcome, .. } => !matches!(outcome.as_str(), "done" | "steered" | "unmet"),
    };
    let notes_live = joined(&[direction_live.clone(), done_live.clone(), open_live.clone(), carry_live.clone(), asks.clone(), edit.clone()]);
    let live = joined(&[direction_live, done_live, open_live, carry_live, asks, edit, parallel, crate::verifiers::candidates_text(&env.data, &candidates), skills_live(&env.data, &env.prefix, &catalog, &previous_skills)]);
    // A lead session's plan is written by a frontier planning agent (it reads the code, changes
    // nothing, and answers with this same plan); when none can, the local planner writes it.
    // Work that must stay on this computer (ROUTE's `private`, or a plan whose tasks were all held
    // to the local model) is not shown to a hosted planner: the local planner writes it.
    let private = private_work(journal, &env.run_id) || previous.as_ref().is_some_and(|previous| !previous.tasks.is_empty() && previous.tasks.iter().all(|task| task.local));
    let mut planned: Option<PlanOutput> = None;
    if env.lead && !private {
        match frontier_plan(env, journal, &shape, &live).await {
            Ok(output) => planned = Some(output),
            Err(error) if error.code == codes::CANCELLED => return Err(error),
            Err(error) => {
                journal.note(&env.run_id, "frontier_plan_failed", &env.data.text("frontier_plan_failed", &[("error", &error.message)]), false).await?;
            }
        }
    }
    let output = match planned {
        Some(output) => output,
        None => {
            if think {
                plan_notes(env, journal, &notes_live).await?;
            }
            decide_json::<PlanOutput>(env, journal, stage, &shape, &live).await?.0
        }
    };
    let (mut doc, mut tasks) = plan::from_output(&output, max, plan::Known { skills: &catalog, skills_max });
    // Private work: every task of it stays with the local model.
    if private {
        tasks.iter_mut().for_each(|task| task.local = true);
    }
    // A plan written again keeps what the work is for. The goal is the one the user's request
    // set; the approach and the architecture stay too unless a task ended badly (then the approach
    // is what changes). Only the user's own words (a steer) rewrite them: written again without
    // thinking at every review, they drifted from the request (seen: the goal reworded at each
    // replan until the run the user asked for was no longer in it).
    if let (PlanMode::Replan { outcome, .. }, Some(previous)) = (&mode, &previous)
        && outcome != "steered"
    {
        let keep = |now: &mut String, before: &String| {
            if !before.trim().is_empty() {
                *now = before.clone();
            }
        };
        keep(&mut doc.goal, &previous.doc.goal);
        if matches!(outcome.as_str(), "done" | "unmet") {
            keep(&mut doc.approach, &previous.doc.approach);
            keep(&mut doc.architecture, &previous.doc.architecture);
        }
    }
    let mut tasks = plan::merge_same_touches(tasks);
    let (restored, mut dropped) = plan::carry_pending(&pending, &output, &mut doc, &mut tasks);
    if let (PlanMode::Asked, Some(previous)) = (&mode, &previous) {
        plan::keep_dependencies(previous, &mut tasks);
    }
    // A task that redoes finished work (the same title and the same files as a done task) is not
    // done twice (seen: a second plan wrote the adapter, its SGD and the transport again, and three
    // agents redid what three others had finished). Both must be the same: a new change to a file
    // an earlier task touched is new work under another title, and with either one enough, a
    // follow-up task on the one file an earlier task had changed was left out without a word. Each
    // task left out is recorded with why, so the report of the work names it.
    if !done.is_empty() {
        let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
        let redone: Vec<(u32, String)> = tasks
            .iter()
            .filter(|task| {
                let files: HashSet<String> = plan::touched_files(task).into_iter().collect();
                done.iter().any(|(title, done_files)| same(title, &task.title) && files == done_files.iter().cloned().collect::<HashSet<String>>())
            })
            .map(|task| (task.n, task.title.clone()))
            .collect();
        if !redone.is_empty() && redone.len() < tasks.len() {
            let why = env.data.text("plan_redone_why", &[]);
            dropped.extend(redone.iter().map(|(_, title)| format!("{title}: {why}")));
            tasks = plan::drop_tasks(tasks, &redone.iter().map(|(n, _)| *n).collect::<HashSet<u32>>());
        }
    }
    wait_where_tasks_meet(env, &mut tasks).await;
    // Tasks are small parts, each checked on its own (the planner's call); parts that change the
    // same files wait for each other.
    let mut made = match (&mode, &previous) {
        (PlanMode::Replan { outcome, .. }, Some(previous)) => {
            let mut replanned = plan::replanned(previous, doc, tasks, output.note.trim());
            // Only a task that ended badly uses up the replans. The user's word is not a failure;
            // a review after a task that ended well and an ask not shown met each have their own
            // count (`limits.reviews`, `limits.accepts`).
            match outcome.as_str() {
                "steered" => replanned.replans = previous.replans,
                "done" => {
                    replanned.replans = previous.replans;
                    replanned.reviews = previous.reviews + 1;
                }
                "unmet" => {
                    replanned.replans = previous.replans;
                    replanned.accepts = previous.accepts + 1;
                }
                _ => {}
            }
            replanned
        }
        _ => plan::Plan {
            plan_id: open.as_ref().map(|(_, open)| open.plan_id.clone()).unwrap_or_else(plan::new_id),
            version: open.as_ref().map(|(_, open)| open.version + 1).unwrap_or(1),
            doc,
            tasks,
            status: if auto { plan_status::IMPLEMENTING.into() } else { plan_status::DRAFT.into() },
            diff: None,
            note: output.note.trim().to_owned(),
            root_task: None,
            steps_done: Vec::new(),
            replans: 0,
            reviews: 0,
            accepts: 0,
            by: String::new(),
            replaces: Vec::new(),
            // The plan whose place it takes; a next version of the open plan keeps what that
            // plan continues.
            continues: earlier.as_ref().map(|earlier| earlier.plan.plan_id.clone()).or_else(|| open.as_ref().and_then(|(_, open)| open.continues.clone())),
        },
    };
    match &previous {
        Some(previous) => {
            let mut diff = plan::diff(previous, &made);
            diff.tasks_dropped = dropped;
            diff.tasks_restored = restored;
            made.diff = Some(diff);
        }
        // A first version has no difference to show, only what the host left out of it or put back.
        None if !dropped.is_empty() || !restored.is_empty() => {
            made.diff = Some(plan::PlanDiff { tasks_dropped: dropped, tasks_restored: restored, ..Default::default() });
        }
        None => {}
    }
    if matches!(mode, PlanMode::Replan { .. }) {
        final_check_task(env, journal, &mut made);
    }
    made.replaces = open_work.iter().filter(|item| output.replaces.contains(&item.label())).map(|item| item.task.id.clone()).collect();
    // The earlier plan's work stops with this plan's start, whatever the planner named: its agents'
    // tasks, and its tasks not started yet (so none starts later beside this plan).
    if let Some(earlier) = &earlier {
        for id in earlier.work.iter().chain(earlier.pending.iter().filter_map(|task| task.task_id.as_ref())) {
            if !made.replaces.contains(id) {
                made.replaces.push(id.clone());
            }
        }
    }
    // A runner job's plan is the manager's: a replan is offered to it and written only when it
    // says so.
    if let PlanMode::Replan { failed, outcome, summary, .. } = &mode
        && crate::runner::on(&entries)
        && !crate::runner::hold(env, journal, &made, *failed, outcome, summary).await?
    {
        journal.note(&env.run_id, "replan_refused", &env.data.text("replan_refused", &[]), false).await?;
        return Ok(false);
    }
    let seq = journal.append(chatflow_kinds::PLAN, made.entry(&env.run_id)).await?;
    if auto {
        journal.set(seq, "implement_run", json!(env.run_id)).await?;
        // Carried out now: what it takes the place of stops now.
        stop_replaced_work(env, journal, seq, &made).await?;
    }
    if let Some(previous) = &previous {
        cancel_replaced_tasks(env, previous, &made).await;
    }
    Ok(true)
}

/// The plan of a lead session, written by a frontier planning agent (`agents.plan`): what to plan
/// is the user's request as ROUTE stated it (or their latest message), with everything the local
/// planner would be told (`live`), and the answer is an object of the plan's own schema.
async fn frontier_plan(env: &RunEnv, journal: &mut Journal, shape: &Shape, live: &str) -> Result<PlanOutput, RpcError> {
    let entries = journal.entries_vec();
    let route = entries.iter().rev().find(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "run_id") == env.run_id && str_field(&entry.data, "stage") == "route");
    let work = route.and_then(|entry| entry.data.get("detail")).map(|detail| str_field(detail, "work").trim().to_owned()).unwrap_or_default();
    let message = entries.iter().rev().find(|entry| entry.kind == kinds::USER_MESSAGE).map(|entry| str_field(&entry.data, "text").trim().to_owned()).unwrap_or_default();
    let goal = match (work.is_empty(), message.is_empty()) {
        (false, false) => env.data.text("frontier_plan_goal", &[("work", &work), ("message", &message)]),
        (false, true) => work,
        _ => message,
    };
    let params = json!({
        "project_id": env.scope.project_id,
        "from_session": env.session_id,
        "goal": goal,
        "context": live,
        "schema": schemas::json_schema(shape),
    });
    let any = env.stops.any();
    let value = env.host.call_with(agents_methods::PLAN, params, crate::host::Effect::Effect, Some(&any)).await?;
    let result: cc_protocol::contracts::agents::FrontierPlanResult = cc_protocol::parse_params(value)?;
    serde_json::from_value::<PlanOutput>(result.plan).map_err(|error| RpcError::new(codes::INTERNAL, format!("the planning agent's plan could not be read: {error}")))
}

/// The stage of the planner's thinking before a plan is written ([`plan_notes`]).
pub(crate) const PLAN_NOTES: &str = "PLAN_NOTES";

/// PLAN_NOTES: the planner thinks the work through before the constrained PLAN call writes it:
/// one free call with thinking on and no tool use (the tools stay listed so the system block
/// matches the other calls'; `none` refuses the tool-call token). Its answer streams into the
/// transcript as it is written and is the newest thing in the conversation when the plan is
/// written (seen: a plan for a whole system decided in one constrained generation, with a
/// 1500-character field as its only thought). `live`: what the plan is made from (the direction,
/// the asks, why it is written again). Off when the stage is not in `stages.toml`. Written once per
/// plan: a resumed run finds its notes after everything the plan is made from.
async fn plan_notes(env: &RunEnv, journal: &mut Journal, live: &str) -> Result<(), RpcError> {
    if env.data.stage(PLAN_NOTES).is_none() {
        return Ok(());
    }
    let entries = journal.entries_vec();
    let run = of_run(&entries, &env.run_id);
    let since = run
        .iter()
        .filter(|entry| entry.kind == kinds::ROUTE || entry.kind == chatflow_kinds::PLAN || entry.kind == kinds::TOOL_RESULT || entry.kind == kinds::STEER)
        .map(|entry| entry.seq)
        .max()
        .unwrap_or(0);
    let written = run.iter().any(|entry| {
        entry.seq > since
            && entry.kind == kinds::ASSISTANT_MESSAGE
            && str_field(&entry.data, "stage") == PLAN_NOTES
            && entry.data.get("streaming").and_then(Value::as_bool) == Some(false)
            && entry.data.get("interrupted").and_then(Value::as_bool) != Some(true)
            && !str_field(&entry.data, "text").trim().is_empty()
    });
    if written {
        return Ok(());
    }
    let messages = crate::fit::fitted(env, journal, |entries| {
        let mut messages = base_messages(env, entries);
        messages.push(prompts::stage_message(&env.data, &env.prefix.role, PLAN_NOTES, live));
        messages
    })
    .await?;
    let call = env.call(PLAN_NOTES, &env.data.effort.default, messages, None, Some(json!("none")), next_step(journal, &env.run_id), true);
    let any = env.stops.any();
    llm::chat(journal, call, &any).await.map(|_| ())
}

/// An earlier plan of this conversation still at work in the folder when a new plan is about to
/// be carried out.
struct EarlierPlan {
    /// Its newest version.
    plan: plan::Plan,
    /// Its tasks still to do (neither done nor cancelled; the host's own closing and build tasks
    /// are made again for the new plan, not carried).
    pending: Vec<plan::PlanTask>,
    /// The tasks agents of this conversation work on for any plan (memory task ids).
    work: Vec<String>,
}

/// The newest plan of this conversation one of whose tasks an agent still works on, with its tasks
/// still to do; None when no agent works for a plan. Structural: the plans' own task records and
/// the agents' records, as the memory and agents plugins hold them now.
async fn earlier_at_work(env: &RunEnv, entries: &[JournalEntry], open_work: &[crate::manage::OpenWork]) -> Option<EarlierPlan> {
    let planned: HashSet<String> = plan::all(entries).into_iter().flat_map(|plan| plan.tasks.into_iter().filter_map(|task| task.task_id)).collect();
    let work: Vec<String> = open_work.iter().map(|item| item.task.id.clone()).filter(|id| planned.contains(id)).collect();
    if work.is_empty() {
        return None;
    }
    let plan_id = entries.iter().rev().filter_map(plan::parse).find(|plan| plan.tasks.iter().any(|task| task.task_id.as_ref().is_some_and(|id| work.contains(id))))?.plan_id;
    let (_, plan) = plan::version_of(entries, &plan_id)?;
    let host_titles = [env.data.text("baseline_task_title", &[]), env.data.text("build_task_title", &[])];
    let mut pending = Vec::new();
    for task in plan.tasks.iter().filter(|task| !host_titles.contains(&task.title)) {
        let status = match &task.task_id {
            Some(id) => env.host.get::<Task>(memory_methods::TASK_GET, json!({"id": id})).await.map(|record| record.status).unwrap_or_default(),
            None => String::new(),
        };
        if !matches!(status.as_str(), "done" | "cancelled") {
            pending.push(task.clone());
        }
    }
    Some(EarlierPlan { plan, pending, work })
}

/// A replanned plan's last task, made again by the host: every check the tasks still to do pick,
/// with what failed before any change, run until each passes. None when they pick no check (the
/// user's word that the work is judged by its own runs, not by a test suite, holds).
fn final_check_task(env: &RunEnv, journal: &Journal, made: &mut plan::Plan) {
    let fix_title = env.data.text("baseline_task_title", &[]);
    let kept = made.steps_done.len();
    // The planner's copy of it, written last, gives way to the host's.
    while made.tasks.len() > kept && made.tasks.last().is_some_and(|task| task.title == fix_title) {
        made.tasks.pop();
    }
    let open: Vec<&plan::PlanTask> = made.tasks.iter().skip(kept).collect();
    if open.is_empty() {
        return;
    }
    let failures = crate::checks::baseline_failures(journal, &env.run_id);
    let mut every_check: Vec<String> = Vec::new();
    for check in open.iter().flat_map(|task| &task.checks).chain(failures.iter().map(|(command, _)| command)) {
        if !every_check.contains(check) {
            every_check.push(check.clone());
        }
    }
    if every_check.is_empty() {
        return;
    }
    let n = made.tasks.len() as u32 + 1;
    let brief = if failures.is_empty() {
        env.data.text("final_task_brief", &[("checks", &every_check.iter().map(|command| format!("- `{command}`\n")).collect::<String>())])
    } else {
        let lines: String = failures
            .iter()
            .map(|(command, tests)| {
                let tests = match tests {
                    Some(names) if !names.is_empty() => names.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", "),
                    _ => env.data.text("baseline_task_unknown", &[]),
                };
                env.data.text("baseline_task_failure", &[("command", command), ("tests", &tests)])
            })
            .collect();
        env.data.text("baseline_task_brief", &[("failures", &lines)])
    };
    // The fix task of a plan held to the local model stays there too.
    let local = !made.tasks.is_empty() && made.tasks.iter().all(|task| task.local);
    made.tasks.push(plan::PlanTask { n, title: fix_title, brief, checks: every_check, depends_on: ((kept as u32 + 1)..n).collect(), local, ..Default::default() });
}

/// The tasks earlier plans of this conversation made that are done now, each once: its title and
/// the files it changes. Asked of memory (a task's status is its record's).
async fn done_earlier(env: &RunEnv, entries: &[JournalEntry]) -> Vec<(String, Vec<String>)> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut done = Vec::new();
    for earlier in plan::all(entries) {
        for task in earlier.tasks {
            let Some(id) = task.task_id.clone() else { continue };
            if !seen.insert(id.clone()) {
                continue;
            }
            if env.host.get::<Task>(memory_methods::TASK_GET, json!({"id": id})).await.is_ok_and(|record| record.status == "done") {
                done.push((task.title.clone(), plan::touched_files(&task)));
            }
        }
    }
    done
}

/// Each task's dependencies completed from the code graph (structural, [`plan::wait_where_tasks_meet`]):
/// the definitions in the files it changes and the files whose code calls into them. A file that
/// does not exist yet has neither; a failed lookup only leaves the planner's own dependencies.
async fn wait_where_tasks_meet(env: &RunEnv, tasks: &mut [plan::PlanTask]) {
    if tasks.len() < 2 {
        return;
    }
    let mut names: HashMap<u32, Vec<String>> = HashMap::new();
    let mut callers: HashMap<u32, HashSet<String>> = HashMap::new();
    for task in tasks.iter() {
        let files: Vec<String> = plan::touched_files(task).into_iter().filter(|path| std::path::Path::new(&env.scope.workspace).join(path).is_file()).collect();
        let mut defined: Vec<String> = Vec::new();
        for path in &files {
            if let Ok(outline) = env.host.get::<cc_protocol::contracts::memory::OutlineResult>(memory_methods::OUTLINE, crate::impact::scope_args(&env.scope, json!({"path": path}))).await {
                defined.extend(outline.symbols.into_iter().filter(|symbol| !matches!(symbol.kind.as_str(), "file" | "module" | "variable")).map(|symbol| symbol.name));
            }
        }
        names.insert(task.n, plan::names_of(task, &defined));
        if !files.is_empty()
            && let Ok(impact) = env.host.get::<cc_protocol::contracts::memory::ImpactResult>(memory_methods::IMPACT, crate::impact::scope_args(&env.scope, json!({"sids": [], "paths": files, "depth": 1}))).await
        {
            callers.insert(task.n, impact.call_sites.into_iter().map(|site| site.path).filter(|path| !files.contains(path)).collect());
        }
    }
    plan::wait_where_tasks_meet(tasks, &names, &callers);
}

/// The task records of `previous` that its next version `made` does not carry are cancelled: a
/// replaced plan's tasks are not work any more (seen: version 1's tasks left pending in the task
/// list beside version 2's). Best effort: a record left open only shows in the list.
/// Cancelled through the agents plugin, so an agent still working on one stops with it (seen: a
/// replan left the first plan's agents running beside the new ones).
async fn cancel_replaced_tasks(env: &RunEnv, previous: &plan::Plan, made: &plan::Plan) {
    let kept: HashSet<&str> = made.tasks.iter().filter_map(|task| task.task_id.as_deref()).chain(made.root_task.as_deref()).collect();
    // Subtasks first: a root task's cancel would otherwise reach them through the tree anyway.
    let records: Vec<String> = previous.tasks.iter().filter_map(|task| task.task_id.clone()).chain(previous.root_task.clone()).filter(|id| !kept.contains(id.as_str())).collect();
    crate::manage::cancel_tasks(env, &records).await;
}

/// The open background work a plan replaces stops when the plan is carried out (once: the list is
/// cleared on the plan entry); a note says what stopped.
async fn stop_replaced_work(env: &RunEnv, journal: &mut Journal, plan_seq: u64, made: &plan::Plan) -> Result<(), RpcError> {
    if made.replaces.is_empty() {
        return Ok(());
    }
    let stopped = crate::manage::cancel_tasks(env, &made.replaces).await;
    journal.set(plan_seq, "replaces", json!([])).await?;
    let names = if stopped.is_empty() { "-".to_owned() } else { stopped.join(", ") };
    journal.note(&env.run_id, "replaced", &env.data.text("replaced", &[("agents", &names), ("count", &made.replaces.len().to_string())]), true).await.map(|_| ())
}

/// IMPLEMENT: the open plan (or the one the Implement button named) goes to the existing
/// machinery. One task: a DO decision with the task's brief, checks and skills, run by the
/// foreground loop. Several: memory tasks with contracts and dependencies under a root task, the
/// ready ones started with `agents.spawn` (the agents plugin starts the others as their
/// dependencies finish). Resumable: tasks already made are recorded on the plan entry.
async fn implement(env: &RunEnv, journal: &mut Journal, route_seq: u64) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    let named = journal.get(route_seq).map(|entry| str_field(&entry.data, "plan_id").to_owned()).unwrap_or_default();
    let found = if named.is_empty() { plan::latest(&entries) } else { plan::version_of(&entries, &named) };
    let open_work = crate::manage::open_work(env, &entries).await;
    // Stopped before its tasks were done, with nothing at work ([`plan::stopped`]): it starts again
    // as it stands.
    let stopped = open_work.is_empty() && found.as_ref().is_some_and(|(seq, _)| plan::stopped(&entries).is_some_and(|(at, _)| at == *seq));
    // Open, or already being implemented by this run (a resumed run), or stopped.
    let Some((mut plan_seq, mut current)) = found.filter(|(seq, plan)| {
        plan.is_open() || stopped || journal.get(*seq).is_some_and(|entry| str_field(&entry.data, "implement_run") == env.run_id)
    }) else {
        return journal.note(&env.run_id, "implement_done", &env.data.text("implement_none", &[]), true).await.map(|_| ());
    };
    if stopped && str_field(&journal.get(plan_seq).map(|entry| entry.data.clone()).unwrap_or_default(), "implement_run") != env.run_id {
        (plan_seq, current) = restart_stopped(env, journal, current).await?;
    }
    // One plan is at work in a folder at a time, and the newest wins. Agents still working for a
    // plan written after this one: this plan does not start beside them (seen: "Implement" on an
    // older plan's card while a newer plan's agents worked; two agents did the same task and one
    // deleted the other's files). Agents working for an older plan stop when this one starts.
    if let Some(earlier) = earlier_at_work(env, &entries, &open_work).await.filter(|earlier| earlier.plan.plan_id != current.plan_id) {
        let first = |plan_id: &str| entries.iter().find(|entry| entry.kind == chatflow_kinds::PLAN && str_field(&entry.data, "plan_id") == plan_id).map(|entry| entry.seq).unwrap_or(0);
        if first(&earlier.plan.plan_id) > first(&current.plan_id) {
            let text = env.data.text("implement_newer", &[("plan_id", &current.plan_id), ("newer", &earlier.plan.plan_id), ("goal", earlier.plan.doc.goal.trim())]);
            return journal.note(&env.run_id, "implement_done", &text, true).await.map(|_| ());
        }
        for id in earlier.work.iter().chain(earlier.pending.iter().filter_map(|task| task.task_id.as_ref())) {
            if !current.replaces.contains(id) {
                current.replaces.push(id.clone());
            }
        }
    }
    journal.set(plan_seq, "status", json!(plan_status::IMPLEMENTING)).await?;
    journal.set(plan_seq, "implement_run", json!(env.run_id)).await?;
    stop_replaced_work(env, journal, plan_seq, &current).await?;
    current.replaces.clear();
    // A runner job's plan is carried out here, one task after the other, as a plan made for a DO
    // decision is: one working tree, a REVIEW after each task, the next task after the one before
    // it is verified. The decision carries only the goal; each task's brief, checks and skills
    // are its own. A lead session carries nothing out here: every task goes to an agent.
    if !env.lead && crate::runner::on(&entries) {
        let decision = Decision {
            missing: String::new(),
            route: "DO".into(),
            effort: env.data.effort.default.clone(),
            symbols: Vec::new(),
            files: Vec::new(),
            queries: Vec::new(),
            brief: current.doc.goal.clone(),
            checks: Vec::new(),
            skills: Vec::new(),
            note: String::new(),
        };
        journal.append(kinds::ROUTE, json!({"route": "DO", "stage": "decide", "detail": decision, "plan_id": current.plan_id, "run_id": env.run_id, "by": "host"})).await?;
        // This run's own version (the Act stage finds the plan by its run), after the decision.
        let mut mine = current.clone();
        mine.version = current.version + 1;
        mine.status = plan_status::IMPLEMENTING.into();
        mine.diff = None;
        let seq = journal.append(chatflow_kinds::PLAN, mine.entry(&env.run_id)).await?;
        return journal.set(seq, "implement_run", json!(env.run_id)).await;
    }
    if !env.lead && current.tasks.len() <= 1 {
        let mut decision = plan::foreground_decision(&env.data, &current, &env.data.effort.default);
        decision.brief = format!("{}{}", direction(&env.data, &journal.entries_vec()), decision.brief);
        return journal
            .append(kinds::ROUTE, json!({"route": "DO", "stage": "implement", "detail": decision, "plan_id": current.plan_id, "run_id": env.run_id}))
            .await
            .map(|_| ());
    }
    match implement_by_agents(env, journal, plan_seq, &mut current, true).await {
        Ok(text) => journal.note(&env.run_id, "implement_done", &text, true).await.map(|_| ()),
        Err(error) if error.code == codes::CANCELLED => Err(error),
        Err(error) => {
            journal.set(plan_seq, "status", json!(plan_status::APPROVED)).await?;
            journal.note(&env.run_id, "implement_done", &env.data.text("implement_failed", &[("error", &error.message)]), true).await.map(|_| ())
        }
    }
}

/// Whether the run's route said its work must stay on this computer (ROUTE's `private`).
fn private_work(journal: &Journal, run_id: &str) -> bool {
    journal
        .entries()
        .filter(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "stage") == "route" && str_field(&entry.data, "run_id") == run_id)
        .last()
        .is_some_and(|route| route.data.get("private").and_then(Value::as_bool) == Some(true))
}

/// The plan's tasks as memory tasks under a root task, made once: each is recorded on the plan
/// entry as it is made, so a resumed run never makes a task twice. `last_checks` join the last
/// task's contract (a foreground plan: the decision's own checks, verified when the work ends).
/// Returns the root task's id.
async fn materialize_plan_tasks(env: &RunEnv, journal: &mut Journal, plan_seq: u64, current: &mut plan::Plan, last_checks: &[String]) -> Result<String, RpcError> {
    let root = match current.root_task.clone() {
        Some(root) => root,
        None => {
            let root: Task = env
                .host
                .effect(
                    memory_methods::TASK_CREATE,
                    json!({
                        "project_id": env.scope.project_id,
                        "title": current.doc.goal.lines().next().unwrap_or("Plan").trim(),
                        "brief": plan::render(&env.data, current),
                        "plan": current.doc.steps.iter().map(|step| env.data.text("list_item", &[("text", step)])).collect::<Vec<_>>().join("\n"),
                        "contract": cc_protocol::contracts::memory::Contract::default(),
                    }),
                )
                .await
                .and_then(cc_protocol::parse_params::<Task>)?;
            current.root_task = Some(root.id.clone());
            journal.set(plan_seq, "root_task", json!(root.id)).await?;
            journal.append(kinds::TASK_LINK, json!({"task_id": root.id, "title": root.title, "run_id": env.run_id, "plan_id": current.plan_id})).await?;
            root.id
        }
    };
    // What failed before any change is one task of its own, the last, waiting for all the others:
    // they are done when they make nothing worse (`checks::awaited`), and it leaves every check
    // passing. Added once, before any task is made.
    let fix_title = env.data.text("baseline_task_title", &[]);
    // Each other task hears what already fails and which task fixes it, so it neither tries to fix
    // it nor stops on it (seen: an agent spent 17 steps on a contract the baseline already failed).
    // The last task also owns what the other tasks leave failing outside their own files (each of
    // them is judged by the errors in what it changed, `checks::not_its_failures`), so it is there
    // whenever the plan has checks, and its contract is every check of the plan.
    let mut already_failing = String::new();
    if current.tasks.iter().all(|task| task.task_id.is_none()) && current.tasks.last().is_some_and(|task| task.title != fix_title) {
        let failures = crate::checks::baseline_failures(journal, &env.run_id);
        let mut every_check: Vec<String> = Vec::new();
        for check in current.tasks.iter().flat_map(|task| &task.checks).chain(failures.iter().map(|(command, _)| command)) {
            if !every_check.contains(check) {
                every_check.push(check.clone());
            }
        }
        if failures.is_empty() && !every_check.is_empty() {
            let n = current.tasks.len() as u32 + 1;
            current.tasks.push(plan::PlanTask {
                n,
                title: fix_title.clone(),
                brief: env.data.text("final_task_brief", &[("checks", &every_check.iter().map(|command| format!("- `{command}`\n")).collect::<String>())]),
                checks: every_check.clone(),
                depends_on: (1..n).collect(),
                ..Default::default()
            });
            journal.set(plan_seq, "tasks", serde_json::to_value(&current.tasks)?).await?;
        }
        // A check that fails without naming failing tests did not get as far as running them: the
        // project does not build. Every task waits for one that makes it build, first (seen: four
        // agents started on a crate with 62 compile errors; none could run its own work, so they
        // read instead, for hours, while the fix waited as the last task).
        let broken: Vec<&String> = failures.iter().filter(|(_, tests)| tests.as_ref().is_none_or(Vec::is_empty)).map(|(command, _)| command).collect();
        if !broken.is_empty() && current.tasks.first().is_some_and(|task| task.title != env.data.text("build_task_title", &[])) {
            let commands: String = broken.iter().map(|command| format!("- `{command}`\n")).collect();
            plan::put_first(
                &mut current.tasks,
                plan::PlanTask {
                    title: env.data.text("build_task_title", &[]),
                    brief: env.data.text("build_task_brief", &[("checks", &commands)]),
                    checks: broken.iter().map(|command| (*command).clone()).collect(),
                    ..Default::default()
                },
            );
            journal.set(plan_seq, "tasks", serde_json::to_value(&current.tasks)?).await?;
        }
        if !failures.is_empty() {
            let lines: String = failures
                .iter()
                .map(|(command, tests)| {
                    let tests = match tests {
                        Some(names) if !names.is_empty() => names.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", "),
                        _ => env.data.text("baseline_task_unknown", &[]),
                    };
                    env.data.text("baseline_task_failure", &[("command", command), ("tests", &tests)])
                })
                .collect();
            let n = current.tasks.len() as u32 + 1;
            already_failing = env.data.text("baseline_known", &[("failures", &lines), ("n", &n.to_string()), ("title", &fix_title)]);
            current.tasks.push(plan::PlanTask {
                n,
                title: fix_title.clone(),
                brief: env.data.text("baseline_task_brief", &[("failures", &lines)]),
                checks: every_check,
                depends_on: (1..n).collect(),
                ..Default::default()
            });
            journal.set(plan_seq, "tasks", serde_json::to_value(&current.tasks)?).await?;
        }
    }
    let dirs = [env.scope.workspace.as_str(), env.scope.root.as_str()];
    // Each agent works from its brief alone: the direction the user set opens it.
    let direction = direction(&env.data, &journal.entries_vec());
    for index in 0..current.tasks.len() {
        if current.tasks[index].task_id.is_some() {
            continue;
        }
        let task = current.tasks[index].clone();
        let depends: Vec<String> = task.depends_on.iter().filter_map(|n| current.tasks.iter().find(|other| other.n == *n).and_then(|other| other.task_id.clone())).collect();
        let last = index + 1 == current.tasks.len();
        let checks: Vec<String> = task.checks.iter().chain(last_checks.iter().filter(|_| last)).map(|command| contract::rooted(command, &dirs)).collect();
        let made: Task = env
            .host
            .effect(
                memory_methods::TASK_CREATE,
                json!({
                    "project_id": env.scope.project_id,
                    "parent_id": root,
                    "title": task.title,
                    "brief": if task.title == fix_title { format!("{direction}{}", plan::agent_brief(&env.data, current, &task)) } else { format!("{direction}{}{already_failing}{}", plan::agent_brief(&env.data, current, &task), missing_note(env, current, &task).await) },
                    "plan": "",
                    "worker": task.local.then_some(cc_protocol::contracts::agents::workers::LOCAL),
                    "contract": contract::contract_from(&checks),
                    "depends_on": depends,
                    "touches": task.touches,
                }),
            )
            .await
            .and_then(cc_protocol::parse_params::<Task>)?;
        current.tasks[index].task_id = Some(made.id.clone());
        // Recorded at once: a resumed run never makes a task twice.
        journal.set(plan_seq, "tasks", serde_json::to_value(&current.tasks)?).await?;
        journal.append(kinds::TASK_LINK, json!({"task_id": made.id, "title": made.title, "run_id": env.run_id, "parent_id": root, "plan_id": current.plan_id})).await?;
    }
    Ok(root)
}

/// The code names a task's brief relies on that the project does not have and that no task of the
/// plan adds, looked up in the graph (structural): named in the brief, so the agent knows at its
/// first step instead of finding out an hour in (seen: a task built on an engine call that did not
/// exist, found by its agent after sixty minutes of reading). Empty when every name resolves.
async fn missing_note(env: &RunEnv, plan: &plan::Plan, task: &plan::PlanTask) -> String {
    let text = format!("{}\n{}", task.brief, task.known.join("\n"));
    let mut names: Vec<String> = Vec::new();
    for span in text.split('`').skip(1).step_by(2) {
        let span = span.trim();
        let span = span.strip_prefix("fn ").or_else(|| span.strip_prefix("def ")).or_else(|| span.strip_prefix("func ")).unwrap_or(span);
        let name: String = span.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':').collect();
        let name = name.trim_matches(':').to_owned();
        // An identifier, not a path, a command or a value.
        let identifier = name.len() > 3 && name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && (span.len() == name.len() || span[name.len()..].starts_with('('));
        if identifier && !names.contains(&name) {
            names.push(name);
        }
    }
    // A name some task says it adds is planned, not missing.
    let adds = |name: &str| {
        plan.tasks.iter().any(|other| {
            let brief = other.brief.to_lowercase();
            ["add `", "adds `", "create `", "creates `", "new `", "define `", "defines `", "write `", "writes `", "introduce `"]
                .iter()
                .any(|verb| brief.split(verb).skip(1).any(|rest| rest.trim_start_matches("fn ").trim_start_matches("def ").starts_with(&name.to_lowercase())))
        })
    };
    let mut missing = Vec::new();
    for name in names {
        if adds(&name) {
            continue;
        }
        let found = env.host.get::<cc_protocol::contracts::memory::SymbolMatches>(memory_methods::SYMBOL, json!({"project_id": env.scope.project_id, "name": name})).await;
        // Only a lookup that answered "none" counts; a failed lookup says nothing.
        if found.is_ok_and(|found| found.matches.is_empty()) {
            missing.push(format!("`{name}`"));
        }
    }
    if missing.is_empty() { String::new() } else { env.data.text("plan_task_missing", &[("names", &missing.join(", "))]) }
}

/// `baseline`: run the plan's checks first (a skill's units check work that is not there yet, so
/// theirs fail by design and are never a baseline).
/// A stopped plan as its next version, ready to carry out again: the same tasks, the done ones
/// kept (by the plan's own record, or by their task's state when agents carried them out), each
/// other task given a fresh task (its old one was stopped or cancelled). No planner call: nothing
/// about the work changed.
async fn restart_stopped(env: &RunEnv, journal: &mut Journal, mut plan: plan::Plan) -> Result<(u64, plan::Plan), RpcError> {
    let recorded: HashSet<u32> = plan.steps_done.iter().filter(|step| step.outcome == "done").map(|step| step.n).collect();
    for task in plan.tasks.iter_mut().filter(|task| !recorded.contains(&task.n)) {
        let Some(id) = task.task_id.clone() else { continue };
        let finished = env.host.get::<Task>(memory_methods::TASK_GET, json!({"id": id})).await.is_ok_and(|record| record.status == "done");
        if !finished {
            task.task_id = None;
        }
    }
    plan.version += 1;
    plan.status = plan_status::APPROVED.into();
    plan.diff = None;
    plan.note = env.data.text("plan_restarted", &[]);
    let seq = journal.append(chatflow_kinds::PLAN, plan.entry(&env.run_id)).await?;
    Ok((seq, plan))
}

async fn implement_by_agents(env: &RunEnv, journal: &mut Journal, plan_seq: u64, current: &mut plan::Plan, baseline: bool) -> Result<String, RpcError> {
    // The plan's checks on the code before any task: what fails already becomes the last task.
    if baseline && current.tasks.iter().all(|task| task.task_id.is_none()) {
        let mut checks: Vec<String> = Vec::new();
        for check in current.tasks.iter().flat_map(|task| &task.checks) {
            if !checks.contains(check) {
                checks.push(check.clone());
            }
        }
        crate::checks::record_baseline(env, journal, &checks).await?;
    }
    materialize_plan_tasks(env, journal, plan_seq, current, &[]).await?;
    let spawned: HashSet<String> = journal.entries().filter(|entry| entry.kind == kinds::AGENT_SPAWNED).map(|entry| str_field(&entry.data, "task_id").to_owned()).collect();
    // The tasks done before (a plan written again after its tree finished keeps them): only the
    // ones still to do are started and listed, and one that waits only for done tasks is ready.
    let done: HashSet<u32> = current.steps_done.iter().filter(|step| step.outcome == "done").map(|step| step.n).collect();
    let open = current.tasks.iter().filter(|task| !done.contains(&task.n)).count();
    let mut text = env.data.text("implement_started", &[("plan_id", &current.plan_id), ("version", &current.version.to_string()), ("count", &open.to_string())]);
    let mut started = Vec::new();
    for task in current.tasks.iter().filter(|task| !done.contains(&task.n)) {
        let Some(task_id) = task.task_id.clone() else { continue };
        let ready = task.depends_on.iter().all(|n| done.contains(n));
        if ready && !spawned.contains(&task_id) {
            let record: cc_protocol::contracts::memory::AgentRecord = env
                .host
                .effect(agents_methods::SPAWN, json!({"project_id": env.scope.project_id, "task_id": task_id, "from_session": env.session_id, "mode": env.mode}))
                .await
                .and_then(cc_protocol::parse_params)?;
            started.push(record.name.clone());
        }
        let assignee = String::new();
        let waits = task.depends_on.iter().filter(|n| !done.contains(n)).count();
        let depends = if waits == 0 { String::new() } else { env.data.text("delegated_after", &[("count", &waits.to_string())]) };
        text.push_str(&env.data.text("delegated_task", &[("title", &task.title), ("assignee", &assignee), ("depends", &depends)]));
    }
    if !started.is_empty() {
        text.push_str(&env.data.text("delegated_agents", &[("names", &started.join(", "))]));
    }
    Ok(text)
}

/// How many joined computers could take a session's model calls now (the mesh plugin; 0 without it).
async fn mesh_capacity(env: &RunEnv) -> u64 {
    env.host.get::<Value>(cc_protocol::contracts::mesh::methods::CAPACITY, json!({"tag": crate::chatflow::FLOW})).await.ok().and_then(|capacity| capacity.get("free").and_then(Value::as_u64)).unwrap_or(0)
}

/// Whether agents could take a plan's independent tasks: a joined computer free for the session's
/// model calls, or this machine's agents scheduler allowed two or more agents in flight (its
/// bound `max`, 0 = none; one in flight is the same work as the foreground, one task at a time).
/// The bound, never the limit or the places free now: the limit is a passing guard that steps down
/// while a foreground call waits for the model (the planner's own call included) and lifts again,
/// the agents of the work a plan replaces hold their places until it starts, and tasks past the
/// limit queue and start as places free (seen: a replan written while the plan it replaced still
/// had four agents saw none free, was told "here, one at a time", and chained every task).
async fn agents_can_take(env: &RunEnv) -> bool {
    let local = env
        .host
        .get::<cc_protocol::contracts::agents::SchedulerState>(agents_methods::SCHEDULER, json!({"project_id": env.scope.project_id}))
        .await
        .ok()
        .is_some_and(|state| {
            let max = state.detail.get("bounds").and_then(|bounds| bounds.get("max")).and_then(Value::as_u64).unwrap_or(u64::from(state.max_parallel));
            max == 0 || max >= 2
        });
    local || mesh_capacity(env).await > 0
}

/// The auto plan just written: when two of its tasks could run at the same time and agents can
/// take them ([`agents_can_take`]), every task goes to the agents now (the ready ones start, the
/// rest as what they depend on finishes, each agent's model placed by the mesh), and this turn
/// ends with the delegation note. Otherwise the Act stage carries the plan out here, task by task.
async fn parallel_implementation(env: &RunEnv, journal: &mut Journal) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    let Some((plan_seq, mut current)) = plan::for_run(&entries, &env.run_id) else { return Ok(()) };
    // A lead session hands every plan to agents, one task or many: it does not change files.
    if !env.lead && (current.tasks.len() < 2 || !plan::has_parallel(&current.tasks) || !agents_can_take(env).await) {
        return Ok(());
    }
    if current.tasks.is_empty() {
        return Ok(());
    }
    journal.set(plan_seq, "status", json!(plan_status::IMPLEMENTING)).await?;
    match implement_by_agents(env, journal, plan_seq, &mut current, true).await {
        Ok(text) => journal.note(&env.run_id, "implement_done", &text, true).await.map(|_| ()),
        Err(error) if error.code == codes::CANCELLED => Err(error),
        // A lead session never does the work here: the failure is its answer.
        Err(error) if env.lead => {
            journal.set(plan_seq, "status", json!(plan_status::APPROVED)).await?;
            journal.note(&env.run_id, "implement_done", &env.data.text("implement_failed", &[("error", &error.message)]), true).await.map(|_| ())
        }
        // The agents could not take it: the Act stage does it here, task by task, as without the mesh.
        Err(error) => {
            tracing::warn!(%error, "the plan's tasks could not go to agents; carried out here");
            Ok(())
        }
    }
}

/// A skill of the route whose work falls into independent units (`SkillInfo::units`), with a
/// joined computer free to take them: the host lists the units with the skill's command and
/// makes the plan itself, one task per unit and one last task that finishes the whole (the
/// report), each unit's agent running the skill for its unit only; then the tasks go to the
/// agents. The model plans none of it. Returns whether the work was handed over.
async fn units_delegation(env: &RunEnv, journal: &mut Journal, route_seq: u64) -> Result<bool, RpcError> {
    let Some(route) = journal.get(route_seq).cloned() else { return Ok(false) };
    let Some((uri, (list_command, unit_done))) = skillsel::of_decision(&route).into_iter().find_map(|uri| env.prefix.skill_units.get(&uri).cloned().map(|commands| (uri, commands))) else {
        return Ok(false);
    };
    // A lead session's units go to agents whatever this computer's local capacity is.
    if !env.lead && !agents_can_take(env).await {
        return Ok(false);
    }
    let check = cc_protocol::contracts::memory::ContractCheck { name: "units".to_owned(), kind: "command".to_owned(), spec: list_command, expect: None };
    let ran = crate::contract::run_check(&env.host, &env.scope.workspace, &check, &env.stops.any()).await?;
    if !ran.result.ok {
        return Ok(false);
    }
    let units: Vec<(String, String)> = ran.full_output.lines().filter_map(|line| line.split_once('\t')).map(|(id, title)| (id.trim().to_owned(), title.trim().to_owned())).filter(|(id, _)| !id.is_empty()).collect();
    if units.len() < 2 {
        return Ok(false);
    }
    let work = route.data.get("detail").map(|detail| str_field(detail, "work").to_owned()).unwrap_or_default();
    let done = env.prefix.skill_done.get(&uri).cloned();
    let mut tasks: Vec<plan::PlanTask> = units
        .iter()
        .enumerate()
        .map(|(index, (id, title))| plan::PlanTask {
            n: index as u32 + 1,
            title: format!("{id} {title}"),
            brief: env.data.text("units_task_brief", &[("unit", id), ("title", title), ("skill", &uri)]),
            checks: unit_done.iter().map(|command| command.replace("{unit}", id)).collect(),
            skills: vec![uri.clone()],
            ..Default::default()
        })
        .collect();
    tasks.push(plan::PlanTask {
        n: units.len() as u32 + 1,
        title: env.data.text("units_summary_title", &[]),
        brief: env.data.text("units_summary_brief", &[("skill", &uri)]),
        depends_on: (1..=units.len() as u32).collect(),
        checks: done.into_iter().collect(),
        skills: vec![uri.clone()],
        ..Default::default()
    });
    let mut made = plan::Plan {
        plan_id: plan::new_id(),
        version: 1,
        doc: plan::PlanDoc { goal: work, ..Default::default() },
        tasks,
        status: plan_status::IMPLEMENTING.into(),
        ..Default::default()
    };
    let plan_seq = journal.append(chatflow_kinds::PLAN, made.entry(&env.run_id)).await?;
    journal.set(plan_seq, "implement_run", json!(env.run_id)).await?;
    match implement_by_agents(env, journal, plan_seq, &mut made, false).await {
        Ok(text) => {
            journal.note(&env.run_id, "implement_done", &text, true).await?;
            Ok(true)
        }
        Err(error) if error.code == codes::CANCELLED => Err(error),
        Err(error) => {
            tracing::warn!(%error, "the skill's units could not go to agents; the work runs here");
            journal.set(plan_seq, "status", json!(plan_status::DONE)).await?;
            Ok(false)
        }
    }
}

/// The user set aside the direction given earlier in this conversation: the decisions this
/// conversation recorded (their evidence is one of its journal entries) are withdrawn from the
/// project's knowledge, so neither the knowledge note nor a later session's brief shows them, and
/// one host note says which no longer hold (this session's prompt prefix still lists them; seen:
/// "no new code" decisions from an earlier turn, shown right before the plan, made the plan of a
/// later "start coding" a document and a run). Memory being away withdraws nothing; the note still
/// closes the stage.
async fn set_aside(env: &RunEnv, journal: &mut Journal, route_seq: u64) -> Result<(), RpcError> {
    let own = format!("journal://{}/", env.session_id);
    let listed = env
        .host
        .get::<KnowledgeListResult>(memory_methods::KNOWLEDGE_LIST, json!({"project_id": env.scope.project_id, "kind": "decision"}))
        .await
        .map(|result| result.items)
        .unwrap_or_default();
    let mut withdrawn: Vec<String> = Vec::new();
    for item in listed.iter().filter(|item| item.evidence.iter().any(|uri| uri.starts_with(&own))) {
        let done = env
            .host
            .call(memory_methods::KNOWLEDGE_WITHDRAW, json!({"project_id": env.scope.project_id, "id": item.id, "reason": env.data.text("set_aside_reason", &[]), "by": "user"}))
            .await;
        match done {
            Ok(_) => withdrawn.push(env.data.text("list_item", &[("text", item.text.trim())])),
            Err(error) => tracing::warn!(%error, id = %item.id, "a set-aside decision could not be withdrawn"),
        }
    }
    let (text, shown) = if withdrawn.is_empty() { (String::new(), false) } else { (env.data.text("set_aside", &[("items", &withdrawn.join("\n"))]), true) };
    journal
        .append(kinds::NOTE, json!({"text": text, "purpose": "set_aside", "run_id": env.run_id, "host_message": shown, "decision_seq": route_seq}))
        .await
        .map(|_| ())
}

/// One knowledge item as a host note shows it: `<kind>[ <state>] (<source>[, as of <date>]): <text>
/// knowledge://<id>` (the memory plugin's `linked` lines carry the same shape).
fn knowledge_line(item: &KnowledgeItem) -> String {
    let state = if matches!(item.kind.as_str(), "hypothesis" | "question") { format!(" {}", item.state) } else { String::new() };
    let date = item.as_of.as_deref().map(|date| format!(", as of {date}")).unwrap_or_default();
    format!("{}{state} ({}{date}): {}  knowledge://{}", item.kind, item.source, item.text, item.id)
}

/// What the project knows about the turn's work (docs/PROJECT-KNOWLEDGE-PLAN.md P4) and what
/// earlier runs taught about work like it, as one host note (`purpose: knowledge`) after ROUTE,
/// before the work: a `memory.knowledge.search` of the project with ROUTE's `work` sentence (at
/// most `limits.knowledge_per_turn` items, those whose text the prefix's brief holds left out),
/// then the lessons that fit ([`crate::learn::recall`], at most `limits.lessons_per_turn`). The
/// lessons shown are kept on the note (`lessons`), so learning after the run can tell whether the
/// error each is about came back. Nothing when nothing is found, when memory is away, or when the
/// note exists (a resumed run). Code attached to reads arrives with them
/// (`reads::knowledge_lines`); this is the search by meaning.
async fn knowledge_note(env: &RunEnv, journal: &mut Journal) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    let run = of_run(&entries, &env.run_id);
    let Some(route) = run.iter().find(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "stage") == "route") else { return Ok(()) };
    if note_after(&run, route.seq, "knowledge") {
        return Ok(());
    }
    let route_seq = route.seq;
    let work = route.data.get("detail").map(|detail| str_field(detail, "work").trim().to_owned()).unwrap_or_default();
    if work.is_empty() {
        return Ok(());
    }
    let mut parts: Vec<String> = Vec::new();
    let limit = env.data.limits.knowledge_per_turn;
    if limit > 0
        && let Ok(found) = env.host.get::<KnowledgeSearchResult>(memory_methods::KNOWLEDGE_SEARCH, json!({"project_id": env.scope.project_id, "text": work, "limit": limit})).await
    {
        // The contract's short copy is the system prompt's, never a note (a session whose prompt
        // shows the whole file would get the copy again on top of it).
        let lines: Vec<String> = found
            .items
            .iter()
            .filter(|item| item.kind != cc_protocol::contracts::memory::knowledge::CONTRACT && !item.text.trim().is_empty() && !env.prefix.system.contains(item.text.trim()))
            .map(knowledge_line)
            .collect();
        if !lines.is_empty() {
            parts.push(env.data.text("knowledge_note", &[("text", &lines.join("\n"))]));
        }
    }
    let languages: Vec<String> = journal
        .context_entry()
        .and_then(|entry| entry.data.get("languages").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|language| language.as_str().map(str::to_owned))
        .collect();
    let mut lessons = crate::learn::recall(&env.host, &env.data, &work, &languages).await;
    // A replay (`replay.rs`) shows its lesson first, or never.
    if let Some((arm, lesson, text)) = crate::replay::arm(&entries) {
        lessons.retain(|item| item.id != lesson);
        if arm == "with" && !text.trim().is_empty() {
            lessons.insert(0, cc_protocol::contracts::memory::KnowledgeItem { id: lesson, text, ..Default::default() });
        }
    }
    if !lessons.is_empty() {
        let lines: Vec<String> = lessons.iter().map(|item| env.data.text("list_item", &[("text", item.text.trim())])).collect();
        parts.push(env.data.text("lessons_note", &[("text", &lines.join("\n"))]));
    }
    let procedures = if crate::replay::arm(&entries).is_none() {
        crate::procedures::recall(&env.host, &env.scope.project_id, journal.session_id(), &env.run_id, &work, &languages).await.items
    } else { Vec::new() };
    for procedure in &procedures {
        let steps: Vec<String> = procedure.steps.iter().enumerate().map(|(i, step)| format!("{}. {}", i + 1, step)).collect();
        parts.push(env.data.text("procedure_note", &[("when", &procedure.applicability), ("steps", &steps.join("\n"))]));
    }
    if parts.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = lessons.iter().map(|item| item.id.as_str()).collect();
    journal
        .append(kinds::NOTE, json!({"text": parts.join("\n\n"), "purpose": "knowledge", "run_id": env.run_id, "host_message": true, "decision_seq": route_seq, "lessons": shown, "procedures": procedures.iter().map(|p| &p.key).collect::<Vec<_>>()}))
        .await
        .map(|_| ())
}

/// The code names a turn saw: the route's symbols and files, the host's symbol, outline and file
/// reads, and the model's own `read` and `edit` paths (never URIs). What a KNOWLEDGE item may bind
/// to: the host narrows the choice to code the turn actually looked at, so an item never names code
/// nobody saw. In journal order, each once, all of them: they are short names, and one left out is
/// code an item cannot bind to.
pub(crate) fn turn_binds(entries: &[JournalEntry], run_id: &str, route_seq: u64) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        let name = name.trim();
        if !name.is_empty() && !name.contains("://") && !out.iter().any(|x| x == name) {
            out.push(name.to_owned());
        }
    };
    if let Some(route) = entries.iter().find(|entry| entry.seq == route_seq) {
        for field in ["symbols", "files"] {
            for name in detail_list(route, field) {
                push(&name);
            }
        }
    }
    for entry in entries.iter().filter(|entry| entry.kind == kinds::TOOL_CALL && str_field(&entry.data, "run_id") == run_id) {
        let tool = str_field(&entry.data, "tool");
        let args = entry.data.get("args").cloned().unwrap_or(Value::Null);
        let field = |name: &str| args.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        match tool {
            memory_methods::SYMBOL => push(&field("name")),
            memory_methods::OUTLINE => push(&field("path")),
            tools_methods::READ => push(&field("target")),
            "read" | "edit" => push(&field("path")),
            _ => {}
        }
    }
    out
}

/// KNOWLEDGE: the items the latest message stated about the project's world, kept at once in the
/// project's knowledge (source `user`, the journal entry as evidence, linked to the code they bind)
/// and journaled as `knowledge` entries with their id. The schema's `binds` pick holds only the code
/// the turn saw ([`turn_binds`]); the model's `state` is kept only when the kind allows it.
async fn propose_knowledge(env: &RunEnv, journal: &mut Journal, route_seq: u64) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    let binds = turn_binds(&entries, &env.run_id, route_seq);
    let shape = stage_shape(env, "KNOWLEDGE")?.with_reads(&binds, 2);
    let proposed = match decide_json::<KnowledgeProposed>(env, journal, "KNOWLEDGE", &shape, "").await {
        Ok((proposed, _)) => proposed.items,
        Err(error) if error.code == codes::CANCELLED => return Err(error),
        Err(error) => {
            tracing::warn!(%error, "the KNOWLEDGE output could not be read");
            Vec::new()
        }
    };
    let key = |name: &str| name.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let mut known: Vec<String> = entries
        .iter()
        .filter(|entry| entry.kind == chatflow_kinds::KNOWLEDGE && str_field(&entry.data, "state") != proposal::REJECTED)
        .map(|entry| key(str_field(&entry.data, "name")))
        .collect();
    // Every item the model proposed is kept (the schema says how many it may write).
    for item in proposed {
        let kind = item.kind.trim().to_ascii_lowercase();
        let (name, text) = (item.name.trim(), item.text.trim());
        if !matches!(kind.as_str(), "concept" | "fact" | "hypothesis" | "question") || name.is_empty() || text.is_empty() || known.contains(&key(name)) {
            continue;
        }
        let state = match (kind.as_str(), item.state.trim()) {
            ("hypothesis", state @ ("open" | "supported" | "refuted")) => state,
            ("hypothesis", _) | ("question", _) => "open",
            _ => "asserted",
        };
        let bound: Vec<&str> = item.binds.iter().map(String::as_str).filter(|name| binds.iter().any(|b| b == name)).collect();
        let seq = journal
            .append(
                chatflow_kinds::KNOWLEDGE,
                json!({"kind": kind, "name": name, "text": text, "item_state": state, "binds": bound, "state": proposal::ACCEPTED, "run_id": env.run_id}),
            )
            .await?;
        let rel = if kind == "concept" { "implemented_by" } else { "about" };
        let links: Vec<Value> = bound.iter().map(|name| json!({"rel": rel, "target": name})).collect();
        let upsert = json!({
            "project_id": env.scope.project_id, "kind": kind, "name": name, "text": text, "state": state, "source": "user", "by": "user",
            "evidence": [format!("journal://{}/{}", env.session_id, seq)], "links": links
        });
        match env.host.call(memory_methods::KNOWLEDGE_UPSERT, upsert).await {
            Ok(stored) => {
                if let Some(id) = stored.get("id").and_then(Value::as_str) {
                    journal.set(seq, "knowledge_id", json!(id)).await?;
                }
            }
            Err(error) => tracing::warn!(%error, "a knowledge item was not stored"),
        }
        known.push(key(name));
    }
    journal.note(&env.run_id, "knowledge_done", "", false).await.map(|_| ())
}

/// Whether a plan task's work stands verified: since the task started, a check that passed came
/// after the task's last edit (a task that edited nothing counts from its start). A check is a run
/// the tools read as a test report with tests in it, or a run of one of `checks` (the task's own
/// checks and the project's check commands, also with arguments after them). Any other command
/// that exits 0 shows nothing (seen: a stopped task counted done on an `ls`).
fn verified_since_last_edit(entries: &[JournalEntry], run_id: &str, task_id: &str, checks: &[String]) -> bool {
    let checks: Vec<String> = checks.iter().map(|check| crate::repetition::base_command(check)).filter(|check| !check.is_empty()).collect();
    let is_check = |command: &str, display: &Value| {
        if display.get("tests").is_some_and(Value::is_object) && display.get("no_tests").and_then(Value::as_bool) != Some(true) {
            return true;
        }
        let base = crate::repetition::base_command(command);
        !base.is_empty() && checks.iter().any(|check| base == *check || base.starts_with(&format!("{check} ")))
    };
    let Some(start) = entries
        .iter()
        .rev()
        .find(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == "task_start" && str_field(&entry.data, "task_id") == task_id)
        .map(|entry| entry.seq)
    else {
        return false;
    };
    // call id -> (tool, its command when it is a run)
    let mut tools: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new();
    let mut last_edit = start;
    let mut last_pass = 0;
    for entry in entries.iter().filter(|entry| entry.seq > start) {
        let call_id = str_field(&entry.data, "call_id").to_owned();
        match entry.kind.as_str() {
            kinds::TOOL_CALL if str_field(&entry.data, "run_id") == run_id && entry.data.get("host").and_then(Value::as_bool) != Some(true) => {
                let command = entry.data.get("args").map(|args| str_field(args, "command").to_owned()).unwrap_or_default();
                tools.insert(call_id, (str_field(&entry.data, "tool").to_owned(), command));
            }
            kinds::TOOL_RESULT => match tools.get(&call_id).map(|(tool, command)| (tool.as_str(), command.as_str())) {
                Some(("edit", _)) if entry.data.get("ok").and_then(Value::as_bool) == Some(true) => last_edit = entry.seq,
                Some(("run", command)) => {
                    let display = entry.data.get("display").cloned().unwrap_or(Value::Null);
                    if display.get("exit_code").and_then(Value::as_i64) == Some(0) && is_check(command, &display) {
                        last_pass = entry.seq;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    last_pass > last_edit
}

/// What a foreground task is about: DECIDE writes a `brief` only for a reader outside the
/// conversation (DELEGATE, PLAN; an IMPLEMENT decision carries its plan task's), so a DO decision's
/// task is named by ROUTE's `work`, the turn's own sentence of what the user wants.
fn foreground_brief<'a>(entries: impl Iterator<Item = &'a JournalEntry>, run_id: &str, decision: &Decision) -> String {
    if !decision.brief.trim().is_empty() {
        return decision.brief.trim().to_owned();
    }
    entries
        .filter(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "stage") == "route")
        .last()
        .and_then(|entry| entry.data.get("detail"))
        .map(|detail| str_field(detail, "work").trim().to_owned())
        .unwrap_or_default()
}

/// The task (with its contract) behind a foreground DO decision, created once and linked.
async fn foreground_task(env: &RunEnv, journal: &mut Journal, decision: &Decision) -> Result<Option<Task>, RpcError> {
    let proposed = contract::contract_from(&decision.checks);
    if proposed.checks.is_empty() {
        return Ok(None);
    }
    let brief = foreground_brief(journal.entries(), &env.run_id, decision);
    let linked = journal
        .entries()
        .filter(|entry| entry.kind == kinds::TASK_LINK && str_field(&entry.data, "run_id") == env.run_id)
        .map(|entry| str_field(&entry.data, "task_id").to_owned())
        .last();
    if let Some(task_id) = linked {
        if let Ok(task) = env.host.get::<Task>(memory_methods::TASK_GET, json!({"id": task_id})).await {
            if task.contract_hash.is_none() {
                // Created but not yet frozen (the run stopped in between): freeze it now.
                return Ok(Some(env.host.get::<Task>(memory_methods::TASK_UPDATE, json!({"id": task.id, "freeze": true})).await.unwrap_or(task)));
            }
            return Ok(Some(task));
        }
        let contract = crate::checks::refine(env, journal, &proposed).await?;
        return Ok(Some(local_task(env, &task_id, &brief, contract)));
    }
    let contract = crate::checks::refine(env, journal, &proposed).await?;
    if contract.checks.is_empty() {
        return Ok(None);
    }
    let title = task_title(&brief);
    let mut touches: Vec<String> = decision.files.clone();
    touches.extend(decision.symbols.iter().cloned());
    let created = env
        .host
        .effect(
            memory_methods::TASK_CREATE,
            json!({"project_id": env.scope.project_id, "title": title, "brief": brief, "plan": "", "contract": contract, "touches": touches}),
        )
        .await
        .and_then(cc_protocol::parse_params::<Task>);
    let task = match created {
        // Claimed by this run: the contract is frozen; from here the run may only propose changes.
        Ok(task) => env
            .host
            .get::<Task>(memory_methods::TASK_UPDATE, json!({"id": task.id, "status": "in_progress", "session_id": env.session_id, "freeze": true}))
            .await
            .unwrap_or(task),
        // Without a task store the contract is still verified by the host, locally.
        Err(error) => {
            tracing::warn!(%error, "memory.task.create failed; verifying the contract locally");
            local_task(env, &format!("local-{}", uuid::Uuid::new_v4().simple()), &brief, contract)
        }
    };
    journal.append(kinds::TASK_LINK, json!({"task_id": task.id, "title": task.title, "run_id": env.run_id})).await?;
    Ok(Some(task))
}

/// A task's title: the first line of what it is about.
fn task_title(brief: &str) -> String {
    brief.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("Foreground change").to_owned()
}

fn local_task(env: &RunEnv, id: &str, brief: &str, contract: cc_protocol::contracts::memory::Contract) -> Task {
    Task {
        id: id.to_owned(),
        project_id: env.scope.project_id.clone(),
        parent_id: None,
        title: task_title(brief),
        brief: brief.to_owned(),
        plan: String::new(),
        contract,
        status: "in_progress".into(),
        assignee: None,
        depends_on: vec![],
        touches: vec![],
        result: None,
        session_id: Some(env.session_id.clone()),
        created_ms: now_ms(),
        updated_ms: now_ms(),
        contract_hash: None,
        contract_revisions: vec![],
        effort: None,
        worker: None,
    }
}

/// A text without its lines that point to a kept output (`read artifact://<id>...`), which differ
/// on every run of the same command.
fn without_pointers(text: &str) -> String {
    text.lines().filter(|line| !line.contains("artifact://")).collect::<Vec<_>>().join("\n").trim().to_owned()
}

/// Rounds in a row the `done` output may stay the same before the turn ends: one round without a
/// settled line is a unit the model is still reading (seen: 68 lines read, then a progress reply);
/// two is the model repeating itself.
const WORK_OPEN_STALL_ROUNDS: usize = 2;

/// A user message of at most this many characters is shown again right before the ROUTE decision.
/// Short messages are the ones the router answered from the turn before ("so?", "continue", "as i
/// said stop"); a longer one states its own asks.
const ROUTE_LATEST_CHARS: usize = 400;
/// The output of the `done` command of a skill this run used, when it says the work is open (a
/// non-zero exit); None when no used skill declares one, when every one says finished, or when the
/// output has not changed for [`WORK_OPEN_STALL_ROUNDS`] rounds (no progress: the model would only
/// repeat itself).
async fn work_open(env: &RunEnv, journal: &Journal) -> Result<Option<String>, RpcError> {
    let entries = journal.entries_vec();
    let used = skillsel::used_by_run(&entries, &env.run_id);
    let commands: Vec<(&String, &String)> = used.iter().filter_map(|uri| env.prefix.skill_done.get(uri).map(|command| (uri, command))).collect();
    if commands.is_empty() {
        return Ok(None);
    }
    let previous: Vec<String> = entries
        .iter()
        .rev()
        .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == env.run_id && str_field(&entry.data, "purpose") == "work_open")
        .map(|entry| str_field(&entry.data, "output").to_owned())
        .take(WORK_OPEN_STALL_ROUNDS)
        .collect();
    for (_, command) in commands {
        let check = cc_protocol::contracts::memory::ContractCheck { name: "done".to_owned(), kind: "command".to_owned(), spec: command.clone(), expect: None };
        let ran = crate::contract::run_check(&env.host, &env.scope.workspace, &check, &env.stops.any()).await?;
        if ran.result.indeterminate {
            continue;
        }
        if !ran.result.ok {
            // What the model acts on, as a failed check shows it: the failing tests or the errors
            // whole with the pointer to the whole output, never the run's head and tail.
            // (`run_check` made it: `output_tail`.) Compared without the pointer line: every run keeps
            // its output under a new id, so a pointer never matches the round before.
            let shown = ran.result.output_tail.clone();
            let output = without_pointers(&shown);
            if previous.len() >= WORK_OPEN_STALL_ROUNDS && previous.iter().all(|earlier| without_pointers(earlier) == output) {
                tracing::info!(session = %env.session_id, rounds = WORK_OPEN_STALL_ROUNDS, "work still open with no progress for several rounds; the turn ends");
                return Ok(None);
            }
            return Ok(Some(shown));
        }
    }
    Ok(None)
}

/// What ROUTE read the user's latest message to ask (its `asks`), numbered, in the stage text
/// `text`: the closing lines of a stage that answers the message. The message itself can be far
/// above by then (seen: 30k tokens of agent reports between it and REPLY; the answer reported
/// those and left the question unanswered). `means`: each quote with what ROUTE read it to refer
/// to, for an answer; a planner gets the quotes alone (seen: "a real-world test suite" read as
/// "test the brain's recall", the old design the user had just rejected).
pub(crate) fn asks_live(data: &crate::data::Dataset, entries: &[JournalEntry], run_id: &str, text: &str, means: bool) -> String {
    let Some(route) = entries.iter().find(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "stage") == "route") else {
        return String::new();
    };
    let asks: Vec<schemas::Ask> = route.data.get("detail").and_then(|detail| detail.get("asks")).cloned().and_then(|asks| serde_json::from_value(asks).ok()).unwrap_or_default();
    let lines: Vec<String> = asks
        .iter()
        .filter(|ask| !ask.quote.trim().is_empty())
        .map(|ask| {
            let (quote, read) = (ask.quote.trim(), ask.means.trim());
            if !means || read.is_empty() || read.eq_ignore_ascii_case(quote) { data.text("ask_line", &[("quote", quote)]) } else { data.text("ask_line_means", &[("quote", quote), ("means", read)]) }
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let list = lines.iter().enumerate().map(|(index, line)| format!("{}. {line}", index + 1)).collect::<Vec<_>>().join("\n");
    data.text(text, &[("asks", &list)])
}

/// The closing lines of a STATUS answer: the user's question and the step to take with the agents,
/// tasks and board the host read. Never empty: a stage message that only names its stage gives the
/// model nothing to do but repeat it (seen: the whole reply was "[host] Stage: ASK.").
pub(crate) fn status_live(data: &crate::data::Dataset, entries: &[JournalEntry], run_id: &str) -> String {
    let asks = asks_live(data, entries, run_id, "status_asks", true);
    if asks.is_empty() { data.text("status_step", &[]) } else { asks }
}

/// Whether ROUTE sent this run to `route`.
pub(crate) fn routed(entries: &[JournalEntry], run_id: &str, route: &str) -> bool {
    entries.iter().any(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "stage") == "route" && str_field(&entry.data, "route") == route)
}

/// The direction the user set for the whole work: the latest message ROUTE read as laying it out
/// (`direction: states`: the message), or as accepting the design in the answer before it
/// (`approves`: that answer and the message); a later message that sets the direction aside
/// (`drops`) ends it. Empty when there is none. Shown before every plan and task: the project's
/// rules file and the earlier work are in every prompt, while the user's change of course was
/// only in the conversation, shortened as it grew (seen: an approved design, "the brain steers
/// the model, it stores no facts", built and tested as a fact-recall memory again for two days).
pub(crate) fn direction(data: &crate::data::Dataset, entries: &[JournalEntry]) -> String {
    let mut current = String::new();
    for route in entries.iter().filter(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "stage") == "route") {
        let set = route.data.get("detail").map(|detail| str_field(detail, "direction").to_owned()).unwrap_or_default();
        if set != "states" && set != "approves" {
            if route.data.get("drops").and_then(Value::as_bool) == Some(true) {
                current.clear();
            }
            continue;
        }
        let Some(message) = entries.iter().filter(|entry| entry.seq < route.seq && entry.kind == kinds::USER_MESSAGE).last() else { continue };
        let said = str_field(&message.data, "text").trim();
        let answer = entries
            .iter()
            .filter(|entry| entry.seq < message.seq && entry.kind == kinds::ASSISTANT_MESSAGE && str_field(&entry.data, "stage") != PLAN_NOTES && !str_field(&entry.data, "text").trim().is_empty())
            .last()
            .map(|entry| str_field(&entry.data, "text").trim().to_owned())
            .unwrap_or_default();
        current = if set == "approves" && !answer.is_empty() {
            data.text("direction_approved", &[("said", said), ("answer", &answer)])
        } else {
            data.text("direction_said", &[("said", said)])
        };
    }
    if current.is_empty() { current } else { data.text("direction_live", &[("direction", &current)]) }
}

/// Keeps the asks whose `quote` is in the user's latest message (compared without case,
/// punctuation or spacing): an ask whose words are not there was written from the conversation
/// (seen: "get it" became "confirm the harness is ready", the previous turn's question). When the
/// router wrote asks and none is in the message, the message itself is the one ask. Returns the
/// quotes it dropped.
fn anchor_asks(asks: &mut Vec<schemas::Ask>, message: &str) -> Vec<String> {
    let words = |text: &str| text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).map(str::to_owned).collect::<Vec<_>>();
    let said = words(message);
    // The router fixes the user's spelling ("capeable" quoted as "capable"): a word counts when
    // the message has it, or one a letter apart; a quote stands when most of its words do.
    let near = |a: &str, b: &str| a == b || (a.chars().count() >= 4 && one_letter_apart(a, b));
    let (kept, dropped): (Vec<schemas::Ask>, Vec<schemas::Ask>) = std::mem::take(asks).into_iter().partition(|ask| {
        let quote = words(&ask.quote);
        let found = quote.iter().filter(|word| said.iter().any(|told| near(word, told))).count();
        !quote.is_empty() && found * 5 >= quote.len() * 4
    });
    *asks = kept;
    if asks.is_empty() && !dropped.is_empty() && !message.trim().is_empty() {
        asks.push(schemas::Ask { quote: message.trim().to_owned(), means: String::new() });
    }
    dropped.into_iter().map(|ask| ask.quote).collect()
}

/// Whether two words differ by one letter added, dropped or changed.
fn one_letter_apart(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    if long.len() - short.len() > 1 {
        return false;
    }
    let head = short.iter().zip(long.iter()).take_while(|(x, y)| x == y).count();
    let skip = if short.len() == long.len() { 1 } else { 0 };
    short.get(head + skip..).unwrap_or(&[]) == long.get(head + 1..).unwrap_or(&[])
}

/// True when this run was continued because a tool reports open work (its latest host note).
pub fn work_open_round(entries: &[JournalEntry], run_id: &str) -> bool {
    entries.iter().any(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == "work_open")
}
