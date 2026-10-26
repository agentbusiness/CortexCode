//! The foreground turn, a router-first flow recast for coding:
//! 1. ROUTE: one constrained generation writes `work`, then the route (ANSWER, LOOKUP, DO,
//!    DELEGATE, STATUS, PLAN, IMPLEMENT), then, on the routes that work, the skills whose summary
//!    names that work ([`crate::skillsel`]), and what to read. The chosen skills are injected once,
//!    before anything else of the turn runs.
//! 2. Host reads run concurrently (graph and workspace) and are journaled.
//! 3. DECIDE writes `missing` first, then MORE (another round of reads), ANSWER, DO or DELEGATE.
//! 4. The result: a streamed reply, the coding agent loop, or a delegation to background agents.
//!
//! Around them, the ideation parts (docs/IDEATION-WORKFLOW.md):
//! - right after ROUTE, a message that makes or changes the spec runs SPEC or SPEC_EDIT
//!   ([`crate::spec`]);
//! - DECIDE (or ROUTE) PLAN runs PLAN, or PLAN_EDIT while a plan is open: a plan document and a
//!   task graph, nothing changed ([`crate::plan`]); ROUTE IMPLEMENT (or the plan card's Implement)
//!   hands the open plan to the foreground loop (one task) or to memory tasks and agents (several);
//! - after the answer, a message the router judged to settle something runs DECISIONS, whose
//!   proposals the user accepts or rejects.
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
use crate::schemas::{self, Decision, KnowledgeProposed, PlanOutput, Proposed, RouterDecision, Shape, SpecEdit, SpecExtract, parse_json};
use crate::{ideation, plan, spec};
use cc_protocol::contracts::chatflow::{kinds as chatflow_kinds, plan_status};
use cc_protocol::contracts::agents::{DelegateResult, methods as agents_methods};
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::lang::LanguageInfo;
use cc_protocol::contracts::memory::{KnowledgeItem, KnowledgeSearchResult, Task, methods as memory_methods};
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
    /// The message makes (`amend` false) or changes the spec, right after the route.
    Spec { amend: bool },
    /// PLAN, or PLAN_EDIT while a plan is open, after a decision to plan. `auto`: the plan of a DO
    /// decision, written before the work and carried out at once by the Act stage, step by step.
    Plan { decision_seq: u64, auto: bool },
    /// IMPLEMENT: the open plan goes to the task machinery.
    Implement { route_seq: u64 },
    /// IDEATE: the host grounds the question (a code-graph search for it, the files it names,
    /// what the project knows, earlier notes), before the ideas are written.
    IdeateReads { route_seq: u64 },
    /// IDEATE: one idea per principle, then the host looks each up ([`crate::ideation`]).
    Ideas,
    /// IDEATE: the critique, the options and a recommendation: the turn's answer.
    Options,
    /// After the answer, when the router judged that the message settles something.
    Decisions { route_seq: u64 },
    /// After the answer (and the decisions), when the router judged that the message states
    /// something durable about the project's world (docs/PROJECT-KNOWLEDGE-PLAN.md P3).
    Knowledge { route_seq: u64 },
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
    let work = first.then(|| decision.data.get("detail").map(|detail| str_field(detail, "work").to_owned()).unwrap_or_default());
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

fn spec_done(run: &[&JournalEntry]) -> bool {
    run.iter().any(|entry| entry.kind == chatflow_kinds::SPEC || (entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "spec_done"))
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
    // The skills ROUTE chose, then the spec: everything after the route sees them.
    if !skillsel::of_decision(route).is_empty() && !skillsel::injected(run.iter().copied(), route.seq) {
        return Stage::Skills { route_seq: route.seq };
    }
    let action = str_field(&route.data, "spec_action");
    if matches!(action, "new" | "amend") && !spec_done(&run) {
        return Stage::Spec { amend: action == "amend" };
    }
    let stage = derive_work(entries, run_id, read_rounds_limit);
    if stage == Stage::Done && route.data.get("settles").and_then(Value::as_bool) == Some(true) && !note_after(&run, route.seq, "decisions_done") {
        return Stage::Decisions { route_seq: route.seq };
    }
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
            "STATUS" if reply_finished(&run, last.seq) => Stage::Done,
            "STATUS" if host_reads_after(&run, last.seq) > 0 => Stage::Reply,
            "STATUS" => Stage::StatusReads,
            "IMPLEMENT" if note_after(&run, last.seq, "implement_done") => Stage::Done,
            "IMPLEMENT" => Stage::Implement { route_seq: last.seq },
            // A plan card's Revise names its plan: the comments are the change, PLAN_EDIT writes it.
            "PLAN" if !str_field(&last.data, "plan_id").is_empty() && plan_written(&run, last.seq) => Stage::Done,
            "PLAN" if !str_field(&last.data, "plan_id").is_empty() => Stage::Plan { decision_seq: last.seq, auto: false },
            "IDEATE" => match ideation::phase_after(&run, last.seq).as_deref() {
                Some(ideation::PHASE_OPTIONS) => Stage::Done,
                Some(_) => Stage::Options,
                None if ideation::grounded(&run, last.seq) => Stage::Ideas,
                None => Stage::IdeateReads { route_seq: last.seq },
            },
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
        "DO" if note_after(&run, last.seq, "act_done") => Stage::Done,
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
        if env.stops.park.is_cancelled() {
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
                // IMPLEMENT needs an open plan, amending needs a spec: otherwise the grammar has neither.
                let has_spec = spec::current(&entries).is_some();
                if plan::open(&entries).is_none() {
                    route_shape = route_shape.without("route", "IMPLEMENT");
                }
                if !has_spec {
                    route_shape = route_shape.without("spec", "amend");
                }
                let routed = decide_json::<RouterDecision>(env, journal, "ROUTE", &route_shape, &skills_live).await;
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
                            asks: if text.trim().is_empty() { Vec::new() } else { vec![text.clone()] },
                            work: text,
                            route: "LOOKUP".into(),
                            effort: "medium".into(),
                            symbols: vec![],
                            files: vec![],
                            queries: vec![],
                            focus: vec![],
                            skills: vec![],
                            spec: String::new(),
                            settles: String::new(),
                            teaches: String::new(),
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
                // What the message does to the spec: structural (marked, long) or the router's word.
                let message = entries.iter().rev().find(|entry| entry.kind == kinds::USER_MESSAGE && str_field(&entry.data, "run_id") == env.run_id);
                let marked = message.and_then(|entry| entry.data.get("spec")).and_then(Value::as_bool) == Some(true);
                let chars = message.map(|entry| str_field(&entry.data, "text").chars().count()).unwrap_or(0);
                let spec_action = spec::action(&decision.spec, has_spec, marked, chars, &env.data.limits);
                let settles = decision.settles == "yes";
                let teaches = decision.teaches == "yes";
                journal
                    .append(
                        kinds::ROUTE,
                        json!({"route": route, "stage": "route", "detail": decision, "raw": raw, "run_id": env.run_id, "spec_action": spec_action, "settles": settles, "teaches": teaches}),
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
                reads::run(journal, &env.run_id, &env.scope, planned, &mut done_reads, &any, env.data.limits.reads_per_round).await.map(|_| ())
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
                    .map(|file| reads::file_read(&env.scope, &env.session_id, file))
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
                let spec_live = spec::live_text(&env.data, &entries, "spec_live");
                if !spec_live.is_empty() {
                    live = if live.is_empty() { spec_live } else { format!("{live}\n{spec_live}") };
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
                let messages = crate::fit::fitted(env, journal, |entries| {
                    let mut messages = base_messages(env, entries);
                    messages.push(prompts::stage_message(&env.data, &env.prefix.role, "REPLY", ""));
                    messages
                })
                .await?;
                // A reply runs no tools, so its prompt offers none: with them listed the model
                // wrote a tool call into the answer as text.
                let mut call = env.call("REPLY", &effort, messages, None, None, next_step(journal, &env.run_id), true);
                call.tools = Vec::new();
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
                        let current_spec = spec::current(&entries).map(|(_, spec)| spec);
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
                        materialize_plan_tasks(env, journal, *plan_seq, plan, current_spec.as_ref(), &last_checks).await?;
                        let planned_task = plan.tasks.get(*n as usize - 1).cloned().unwrap_or_default();
                        let task_id = planned_task.task_id.clone().ok_or_else(|| RpcError::internal("the plan's task was not made"))?;
                        let task: Task = env
                            .host
                            .get::<Task>(memory_methods::TASK_UPDATE, json!({"id": task_id, "status": "in_progress", "session_id": env.session_id, "freeze": true}))
                            .await?;
                        let text = env.data.text("do_task_text", &[("title", &planned_task.title), ("brief", &plan::task_focus(&env.data, &planned_task, current_spec.as_ref(), plan.tasks.len() == 1))]);
                        let later: Vec<String> = plan.tasks.iter().filter(|task| task.n > *n).map(|task| task.title.clone()).collect();
                        let goal = plan.doc.goal.clone();
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
                                    .map(|file| reads::file_read(&env.scope, &env.session_id, file)),
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
                        let requirements: Vec<(String, String)> = current_spec
                            .as_ref()
                            .map(|spec| {
                                if plan.tasks.len() == 1 {
                                    spec.live().map(|requirement| (requirement.id.clone(), requirement.text.clone())).collect()
                                } else {
                                    planned_task.covers.iter().filter_map(|id| spec.requirements.iter().find(|requirement| &requirement.id == id)).map(|requirement| (requirement.id.clone(), requirement.text.clone())).collect()
                                }
                            })
                            .unwrap_or_default();
                        (Some(task), Some(agent_loop::StepSpec { n: *n, of: plan.tasks.len() as u32, text, done: plan.steps_done.clone(), goal, later, requirements }))
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
                            // A task whose work is verified as it stands (a command that passed
                            // since its last edit) is done, also when its loop ended with nothing
                            // left to do or was stopped for re-running that same passing check.
                            let task_id = plan.tasks.get(n as usize - 1).and_then(|task| task.task_id.clone()).unwrap_or_default();
                            let verified = verified_since_last_edit(&journal.entries_vec(), &env.run_id, &task_id);
                            let outcome = if verified && (outcome.outcome == "stopped" || (outcome.outcome == "done" && outcome.tool_calls == 0)) {
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
                            done.push(plan::StepDone { n, outcome: outcome_name.clone(), summary: outcome.summary.clone(), changed_files: outcome.changed_files.clone() });
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
                                if plan.replans < env.data.limits.replans
                                    && let Some(found) = review_plan(env, journal, &plan, n).await
                                {
                                    set_stage(env, journal, status_seq, "plan").await?;
                                    write_plan(env, journal, PlanMode::Replan { plan_seq, failed: n, outcome: outcome_name.clone(), summary: found }).await?;
                                }
                                continue;
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
                                write_plan(env, journal, PlanMode::Replan { plan_seq, failed: n, outcome: outcome_name.clone(), summary: outcome.summary.clone() }).await?;
                                continue;
                            }
                            journal.set(plan_seq, "status", json!(if outcome_name == "done" { plan_status::DONE } else { plan_status::IMPLEMENTING })).await?;
                        }
                        journal
                            .append(
                                kinds::NOTE,
                                json!({
                                    "text": act_note(&outcome),
                                    "purpose": "act_done",
                                    "run_id": env.run_id,
                                    "host_message": false,
                                    "outcome": outcome.outcome,
                                    "changed_files": outcome.changed_files,
                                    "verified": outcome.verified,
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
                            journal
                                .append(
                                    kinds::NOTE,
                                    json!({"text": act_note(&outcome), "purpose": "explore_done", "run_id": env.run_id, "host_message": false, "outcome": outcome.outcome}),
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
                    Err(error) => journal
                        .note(&env.run_id, "delegated", &env.data.text("delegation_failed", &[("error", &error.to_string())]), true)
                        .await
                        .map(|_| ()),
                }
            }
            Stage::Spec { amend } => {
                set_stage(env, journal, status_seq, "spec").await?;
                run_spec(env, journal, &entries, amend).await
            }
            Stage::Plan { auto, .. } => {
                set_stage(env, journal, status_seq, "plan").await?;
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
            Stage::IdeateReads { route_seq } => {
                set_stage(env, journal, status_seq, "read").await?;
                let Some(entry) = journal.get(route_seq).cloned() else { return Err(RpcError::internal("route entry vanished")) };
                let planned = round_reads(&env.scope, &env.session_id, &entry, "IDEATE", &env.data.limits, &[]);
                let any = env.stops.any();
                reads::run(journal, &env.run_id, &env.scope, planned, &mut done_reads, &any, env.data.limits.reads_per_round).await?;
                knowledge_note(env, journal).await?;
                let work = entry.data.get("detail").map(|detail| str_field(detail, "work").to_owned()).unwrap_or_default();
                ideation::notes_note(env, journal, &work).await?;
                journal.note(&env.run_id, ideation::GROUNDED, "", false).await.map(|_| ())
            }
            Stage::Ideas => {
                set_stage(env, journal, status_seq, "ideas").await?;
                ideation::run_ideas(env, journal).await
            }
            Stage::Options => {
                set_stage(env, journal, status_seq, "options").await?;
                ideation::run_options(env, journal).await
            }
            Stage::Decisions { .. } => {
                set_stage(env, journal, status_seq, "decisions").await?;
                propose_decisions(env, journal).await
            }
            Stage::Knowledge { route_seq } => {
                set_stage(env, journal, status_seq, "knowledge").await?;
                propose_knowledge(env, journal, route_seq).await
            }
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
        let failed = (exit != Some(0)).then(|| {
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

fn decide_routes(full: Shape, route: &str) -> Shape {
    match route {
        "PLAN" => full.without("route", "DO").without("route", "DELEGATE"),
        // A change keeps ANSWER: a request that can be read two ways is asked back to the user.
        _ => full,
    }
}

fn act_note(outcome: &LoopOutcome) -> String {
    let verified = match outcome.verified {
        Some(true) => " The host's checks pass.",
        Some(false) => " The host's checks do not pass.",
        None => "",
    };
    let files = if outcome.changed_files.is_empty() { "no files".to_owned() } else { outcome.changed_files.join(", ") };
    let calls = if outcome.tool_calls == 1 { "1 tool call".to_owned() } else { format!("{} tool calls", outcome.tool_calls) };
    format!("Work {}: {calls}, changed {files}.{verified}", outcome.outcome)
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

/// SPEC (the message becomes the spec) or SPEC_EDIT (it changes the requirements). The spec helps
/// the work but is not the work: a decision that cannot be read leaves the message as the spec
/// without a checklist, or the spec as it was, and the turn goes on.
async fn run_spec(env: &RunEnv, journal: &mut Journal, entries: &[JournalEntry], amend: bool) -> Result<(), RpcError> {
    let current = spec::current(entries);
    if amend && let Some((_, current)) = current {
        let shape = stage_shape(env, "SPEC_EDIT")?.filled(schemas::REQUIREMENTS_PLACEHOLDER, &current.live_ids(), None);
        let text = match decide_json::<SpecEdit>(env, journal, "SPEC_EDIT", &shape, "").await {
            // The edit applies to the spec as it is now: the user may have edited it meanwhile.
            Ok((edit, _)) => match spec::edited(&env.data, &newest_spec(journal).await?.unwrap_or(current), &edit) {
                Some(next) => return spec::append(journal, &next, Some(&env.run_id)).await.map(|_| ()),
                None => env.data.text("spec_unchanged", &[]),
            },
            Err(error) if error.code == codes::CANCELLED => return Err(error),
            Err(error) => env.data.text("spec_failed", &[("error", &error.message)]),
        };
        return journal.note(&env.run_id, "spec_done", &text, false).await.map(|_| ());
    }
    let Some(message) = entries.iter().rev().find(|entry| entry.kind == kinds::USER_MESSAGE && str_field(&entry.data, "run_id") == env.run_id) else {
        return journal.note(&env.run_id, "spec_done", "", false).await.map(|_| ());
    };
    let text = spec::source_text(&env.data, &env.data.limits, &message.data);
    let shape = stage_shape(env, "SPEC")?;
    let (extract, failed) = match decide_json::<SpecExtract>(env, journal, "SPEC", &shape, "").await {
        Ok((extract, _)) => (extract, None),
        Err(error) if error.code == codes::CANCELLED => return Err(error),
        Err(error) => (SpecExtract::default(), Some(error.message)),
    };
    let previous = newest_spec(journal).await?;
    let mut made = spec::created(&env.data, previous.as_ref(), spec::accepted(&journal.entries_vec()), text, &extract, message.seq);
    if let Some(error) = failed {
        made.changes = env.data.text("spec_failed", &[("error", &error)]);
    }
    spec::append(journal, &made, Some(&env.run_id)).await.map(|_| ())
}

/// The current spec as the journal holds it now (after the stage's model call).
async fn newest_spec(journal: &mut Journal) -> Result<Option<spec::Spec>, RpcError> {
    journal.sync().await?;
    Ok(spec::current(&journal.entries_vec()).map(|(_, spec)| spec))
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
    write_plan(env, journal, if auto { PlanMode::Auto } else { PlanMode::Asked }).await
}

/// PLAN (a new plan) or PLAN_EDIT (a plan's next version, with its difference). Nothing is
/// changed in the repository. An auto plan and a replan are recorded as implementing with the run
/// as `implement_run` (the Act stage carries their tasks out; they are never the open plan a
/// later message edits).
async fn write_plan(env: &RunEnv, journal: &mut Journal, mode: PlanMode) -> Result<(), RpcError> {
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
    let requirement_ids = spec::current(&entries).map(|(_, spec)| spec.live_ids()).unwrap_or_default();
    let max = env.data.limits.plan_max_tasks;
    let previous_skills = skillsel::previous_turn(&entries, &env.run_id);
    let catalog = skillsel::ordered_catalog(&env.prefix.skills, &previous_skills);
    let skills_max = env.data.limits.skills_per_turn;
    // Each task's `checks` is a pick of the project's own check commands (none: no checks).
    let candidates = crate::verifiers::candidates(env, &run_files(&entries, &env.run_id)).await;
    let shape = stage_shape(env, stage)?
        .filled(schemas::REQUIREMENTS_PLACEHOLDER, &requirement_ids, None)
        .filled(schemas::TASKS_PLACEHOLDER, &plan::task_numbers(max), None)
        .filled(schemas::VERIFIERS_PLACEHOLDER, &crate::verifiers::commands(&candidates), None)
        .with_catalog(&catalog, skills_max);
    // The planner is told how its plan is used: carried out at once task by task (auto), written
    // again after a task ended badly, or the next version of the open plan.
    // Joined computers free to run tasks now (the mesh; none without it): the planner hears the
    // number only. An auto plan with free computers runs its independent tasks as agents at once
    // (`parallel_implementation`), so the planner is told that instead of "here, one at a time".
    let free = mesh_capacity(env).await;
    let edit = match &mode {
        PlanMode::Auto if free > 0 => env.data.text("plan_auto_parallel_live", &[("count", &free.to_string())]),
        PlanMode::Auto => env.data.text("plan_auto_live", &[]),
        PlanMode::Replan { failed, outcome, summary, .. } => {
            let plan = previous.clone().unwrap_or_default();
            let done = plan.steps_done.iter().filter(|step| step.outcome == "done").count();
            let title = plan.tasks.get(*failed as usize - 1).map(|task| task.title.clone()).unwrap_or_default();
            // A task that ended well but showed the rest must change (REVIEW), or one that ended badly.
            let key = if outcome == "done" { "plan_revise_live" } else { "plan_replan_live" };
            joined(&[
                env.data.text("plan_auto_live", &[]),
                env.data.text(key, &[("n", &failed.to_string()), ("title", &title), ("outcome", outcome), ("summary", summary.trim()), ("done", &done.to_string())]),
            ])
        }
        PlanMode::Asked => open
            .as_ref()
            .map(|(_, open)| env.data.text("plan_live_edit", &[("plan_id", &open.plan_id), ("version", &open.version.to_string())]))
            .unwrap_or_default(),
    };
    let parallel = if free > 0 && !auto { env.data.text("plan_parallel", &[("count", &free.to_string())]) } else { String::new() };
    // Each task's `skills` choose from the catalog, shown last.
    let live = joined(&[edit, parallel, spec::live_text(&env.data, &entries, "spec_live"), crate::verifiers::candidates_text(&env.data, &candidates), skills_live(&env.data, &env.prefix, &catalog, &previous_skills)]);
    let (output, _) = decide_json::<PlanOutput>(env, journal, stage, &shape, &live).await?;
    let (doc, tasks) = plan::from_output(&output, max, plan::Known { requirements: &requirement_ids, skills: &catalog, skills_max });
    let mut tasks = plan::merge_same_touches(tasks);
    if let (PlanMode::Asked, Some(previous)) = (&mode, &previous) {
        plan::keep_dependencies(previous, &mut tasks);
    }
    // One task is the default: the request is split only where it names several parts (the
    // planner's call); several requirements of one part stay one task.
    let mut made = match (&mode, &previous) {
        (PlanMode::Replan { .. }, Some(previous)) => plan::replanned(previous, doc, tasks, output.note.trim()),
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
            by: String::new(),
        },
    };
    if let Some(previous) = &previous {
        made.diff = Some(plan::diff(previous, &made));
    }
    let seq = journal.append(chatflow_kinds::PLAN, made.entry(&env.run_id)).await?;
    if auto {
        journal.set(seq, "implement_run", json!(env.run_id)).await?;
    }
    if let Some(previous) = &previous {
        cancel_replaced_tasks(env, previous, &made).await;
    }
    Ok(())
}

/// The task records of `previous` that its next version `made` does not carry are cancelled: a
/// replaced plan's tasks are not work any more (seen: version 1's tasks left pending in the task
/// list beside version 2's). Best effort: a record left open only shows in the list.
async fn cancel_replaced_tasks(env: &RunEnv, previous: &plan::Plan, made: &plan::Plan) {
    let kept: HashSet<&str> = made.tasks.iter().filter_map(|task| task.task_id.as_deref()).chain(made.root_task.as_deref()).collect();
    let records = previous.tasks.iter().filter_map(|task| task.task_id.as_deref()).chain(previous.root_task.as_deref());
    for id in records.filter(|id| !kept.contains(id)) {
        if let Err(error) = env.host.call(memory_methods::TASK_UPDATE, json!({"id": id, "status": "cancelled"})).await {
            tracing::warn!(%error, task = id, "cancelling a replaced plan's task failed");
        }
    }
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
    // Open, or already being implemented by this run (a resumed run).
    let Some((plan_seq, mut current)) = found.filter(|(seq, plan)| {
        plan.is_open() || journal.get(*seq).is_some_and(|entry| str_field(&entry.data, "implement_run") == env.run_id)
    }) else {
        return journal.note(&env.run_id, "implement_done", &env.data.text("implement_none", &[]), true).await.map(|_| ());
    };
    journal.set(plan_seq, "status", json!(plan_status::IMPLEMENTING)).await?;
    journal.set(plan_seq, "implement_run", json!(env.run_id)).await?;
    let current_spec = spec::current(&entries).map(|(_, spec)| spec);
    if current.tasks.len() <= 1 {
        let decision = plan::foreground_decision(&env.data, &current, current_spec.as_ref(), &env.data.effort.default);
        return journal
            .append(kinds::ROUTE, json!({"route": "DO", "stage": "implement", "detail": decision, "plan_id": current.plan_id, "run_id": env.run_id}))
            .await
            .map(|_| ());
    }
    match implement_by_agents(env, journal, plan_seq, &mut current, current_spec.as_ref()).await {
        Ok(text) => journal.note(&env.run_id, "implement_done", &text, true).await.map(|_| ()),
        Err(error) if error.code == codes::CANCELLED => Err(error),
        Err(error) => {
            journal.set(plan_seq, "status", json!(plan_status::APPROVED)).await?;
            journal.note(&env.run_id, "implement_done", &env.data.text("implement_failed", &[("error", &error.message)]), true).await.map(|_| ())
        }
    }
}

/// The plan's tasks as memory tasks under a root task, made once: each is recorded on the plan
/// entry as it is made, so a resumed run never makes a task twice. `last_checks` join the last
/// task's contract (a foreground plan: the decision's own checks, verified when the work ends).
/// Returns the root task's id.
async fn materialize_plan_tasks(env: &RunEnv, journal: &mut Journal, plan_seq: u64, current: &mut plan::Plan, current_spec: Option<&spec::Spec>, last_checks: &[String]) -> Result<String, RpcError> {
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
    let dirs = [env.scope.workspace.as_str(), env.scope.root.as_str()];
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
                    "brief": plan::agent_brief(&env.data, current, &task, current_spec),
                    "plan": "",
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

async fn implement_by_agents(env: &RunEnv, journal: &mut Journal, plan_seq: u64, current: &mut plan::Plan, current_spec: Option<&spec::Spec>) -> Result<String, RpcError> {
    materialize_plan_tasks(env, journal, plan_seq, current, current_spec, &[]).await?;
    let spawned: HashSet<String> = journal.entries().filter(|entry| entry.kind == kinds::AGENT_SPAWNED).map(|entry| str_field(&entry.data, "task_id").to_owned()).collect();
    let mut text = env.data.text("implement_started", &[("plan_id", &current.plan_id), ("version", &current.version.to_string()), ("count", &current.tasks.len().to_string())]);
    let mut started = Vec::new();
    for task in &current.tasks {
        let Some(task_id) = task.task_id.clone() else { continue };
        let ready = task.depends_on.is_empty();
        if ready && !spawned.contains(&task_id) {
            let record: cc_protocol::contracts::memory::AgentRecord = env
                .host
                .effect(agents_methods::SPAWN, json!({"project_id": env.scope.project_id, "task_id": task_id, "from_session": env.session_id, "mode": env.mode}))
                .await
                .and_then(cc_protocol::parse_params)?;
            started.push(record.name.clone());
        }
        let assignee = String::new();
        let depends = if task.depends_on.is_empty() { String::new() } else { env.data.text("delegated_after", &[("count", &task.depends_on.len().to_string())]) };
        text.push_str(&env.data.text("delegated_task", &[("title", &task.title), ("assignee", &assignee), ("depends", &depends)]));
    }
    if !started.is_empty() {
        text.push_str(&env.data.text("delegated_agents", &[("names", &started.join(", "))]));
    }
    Ok(text)
}

/// How many joined computers could take a session's model calls now (the mesh plugin; 0 without it).
async fn mesh_capacity(env: &RunEnv) -> u64 {
    env.host.get::<Value>(cc_protocol::contracts::mesh::methods::CAPACITY, json!({})).await.ok().and_then(|capacity| capacity.get("free").and_then(Value::as_u64)).unwrap_or(0)
}

/// The auto plan just written: when two of its tasks could run at the same time and a joined
/// computer is free to take one, every task goes to the agents now (the ready ones start, the
/// rest as what they depend on finishes, each agent's model placed by the mesh), and this turn
/// ends with the delegation note. Otherwise the Act stage carries the plan out here, task by task.
async fn parallel_implementation(env: &RunEnv, journal: &mut Journal) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    let Some((plan_seq, mut current)) = plan::for_run(&entries, &env.run_id) else { return Ok(()) };
    if current.tasks.len() < 2 || !plan::has_parallel(&current.tasks) || mesh_capacity(env).await == 0 {
        return Ok(());
    }
    let current_spec = spec::current(&entries).map(|(_, spec)| spec);
    journal.set(plan_seq, "status", json!(plan_status::IMPLEMENTING)).await?;
    match implement_by_agents(env, journal, plan_seq, &mut current, current_spec.as_ref()).await {
        Ok(text) => journal.note(&env.run_id, "implement_done", &text, true).await.map(|_| ()),
        Err(error) if error.code == codes::CANCELLED => Err(error),
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
    if mesh_capacity(env).await == 0 {
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
    match implement_by_agents(env, journal, plan_seq, &mut made, None).await {
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

/// DECISIONS: what the latest exchange settled, proposed for the user to accept (at most
/// `limits.decisions_per_turn`). A decision that cannot be read proposes nothing.
async fn propose_decisions(env: &RunEnv, journal: &mut Journal) -> Result<(), RpcError> {
    let shape = stage_shape(env, "DECISIONS")?;
    let proposed = match decide_json::<Proposed>(env, journal, "DECISIONS", &shape, "").await {
        Ok((proposed, _)) => proposed.decisions,
        Err(error) if error.code == codes::CANCELLED => return Err(error),
        Err(error) => {
            tracing::warn!(%error, "the DECISIONS output could not be read");
            Vec::new()
        }
    };
    let known: Vec<String> = spec::decisions(&journal.entries_vec()).into_iter().map(|decided| decided.decided).collect();
    let fresh = proposed.iter().filter(|decision| !decision.decided.trim().is_empty() && !known.contains(&decision.decided.trim().to_owned()));
    for decision in fresh.take(env.data.limits.decisions_per_turn) {
        journal.append(chatflow_kinds::DECISION, spec::proposed_entry(&decision.decided, &decision.because, &decision.rejected, &env.run_id)).await?;
    }
    journal.note(&env.run_id, "decisions_done", "", false).await.map(|_| ())
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
        let lines: Vec<String> = found.items.iter().filter(|item| !item.text.trim().is_empty() && !env.prefix.system.contains(item.text.trim())).map(knowledge_line).collect();
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
    if parts.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = lessons.iter().map(|item| item.id.as_str()).collect();
    journal
        .append(kinds::NOTE, json!({"text": parts.join("\n\n"), "purpose": "knowledge", "run_id": env.run_id, "host_message": true, "decision_seq": route_seq, "lessons": shown}))
        .await
        .map(|_| ())
}

/// The code names a turn saw: the route's symbols and files, the host's symbol, outline and file
/// reads, and the model's own `read` and `edit` paths (never URIs). What a KNOWLEDGE item may bind
/// to: the host narrows the choice to code the turn actually looked at, so an item never names code
/// nobody saw. In journal order, each once, at most `max`.
pub(crate) fn turn_binds(entries: &[JournalEntry], run_id: &str, route_seq: u64, max: usize) -> Vec<String> {
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
    out.truncate(max);
    out
}

/// KNOWLEDGE: the items the latest message stated about the project's world, kept at once in the
/// project's knowledge (source `user`, the journal entry as evidence, linked to the code they bind)
/// and journaled as `knowledge` entries with their id. The schema's `binds` pick holds only the code
/// the turn saw ([`turn_binds`]); the model's `state` is kept only when the kind allows it.
async fn propose_knowledge(env: &RunEnv, journal: &mut Journal, route_seq: u64) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    let binds = turn_binds(&entries, &env.run_id, route_seq, 12);
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
        .filter(|entry| entry.kind == chatflow_kinds::KNOWLEDGE && str_field(&entry.data, "state") != spec::REJECTED)
        .map(|entry| key(str_field(&entry.data, "name")))
        .collect();
    let mut count = 0;
    for item in proposed {
        let kind = item.kind.trim().to_ascii_lowercase();
        let (name, text) = (item.name.trim(), item.text.trim());
        if !matches!(kind.as_str(), "concept" | "fact" | "hypothesis" | "question") || name.is_empty() || text.is_empty() || known.contains(&key(name)) {
            continue;
        }
        if count >= env.data.limits.knowledge_items_per_turn {
            break;
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
                json!({"kind": kind, "name": name, "text": text, "item_state": state, "binds": bound, "state": spec::ACCEPTED, "run_id": env.run_id}),
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
        count += 1;
    }
    journal.note(&env.run_id, "knowledge_done", "", false).await.map(|_| ())
}

/// Whether a plan task's work stands verified: since the task started, a `run` that exited 0
/// came after the task's last edit (a task that edited nothing counts from its start).
fn verified_since_last_edit(entries: &[JournalEntry], run_id: &str, task_id: &str) -> bool {
    let Some(start) = entries
        .iter()
        .rev()
        .find(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == "task_start" && str_field(&entry.data, "task_id") == task_id)
        .map(|entry| entry.seq)
    else {
        return false;
    };
    let mut tools: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut last_edit = start;
    let mut last_pass = 0;
    for entry in entries.iter().filter(|entry| entry.seq > start) {
        let call_id = str_field(&entry.data, "call_id").to_owned();
        match entry.kind.as_str() {
            kinds::TOOL_CALL if str_field(&entry.data, "run_id") == run_id && entry.data.get("host").and_then(Value::as_bool) != Some(true) => {
                tools.insert(call_id, str_field(&entry.data, "tool").to_owned());
            }
            kinds::TOOL_RESULT => match tools.get(&call_id).map(String::as_str) {
                Some("edit") if entry.data.get("ok").and_then(Value::as_bool) == Some(true) => last_edit = entry.seq,
                Some("run") if entry.data.get("display").and_then(|display| display.get("exit_code")).and_then(Value::as_i64) == Some(0) => last_pass = entry.seq,
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
    }
}

/// Rounds in a row the `done` output may stay the same before the turn ends: one round without a
/// settled line is a unit the model is still reading (seen: 68 lines read, then a progress reply);
/// two is the model repeating itself.
const WORK_OPEN_STALL_ROUNDS: usize = 2;

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
            let output = ran.full_output.trim();
            if previous.len() >= WORK_OPEN_STALL_ROUNDS && previous.iter().all(|earlier| earlier == output) {
                tracing::info!(session = %env.session_id, rounds = WORK_OPEN_STALL_ROUNDS, "work still open with no progress for several rounds; the turn ends");
                return Ok(None);
            }
            return Ok(Some(ran.full_output));
        }
    }
    Ok(None)
}

/// True when this run was continued because a tool reports open work (its latest host note).
pub fn work_open_round(entries: &[JournalEntry], run_id: &str) -> bool {
    entries.iter().any(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == "work_open")
}
