//! Contract checks are written before any work is done, by a model that has not run anything yet,
//! so a check can be broken in itself (a test runner that is not installed, a wrong argument
//! syntax). The host runs the checks before the work (the baseline, which also shows the agent the
//! failing output it has to fix), and a constrained CHECKS decision keeps, replaces or drops each
//! check from what that output shows. A revision is run again; it is decided again only when a
//! check fails whose output no decision has seen yet (a replacement that passes, or a kept check
//! failing as before, settles the checks without another decision).
//!
//! Once a task is claimed its contract is frozen (memory holds its hash). The worker may then only
//! *propose* a revision, before the work (from the baseline) or after a failure: the planner judges
//! it in a fresh context (`agents.contract.propose`) and only an accepted revision, recorded with
//! the old hash and the reason, replaces the contract. A worker never rewrites its own contract:
//! one that did moved its check into Homebrew's `bin` and copied its files there to pass.
//!
//! The model judges; the host only runs commands.

use crate::agent_loop::RunEnv;
use crate::contract;
use crate::flow::decide_json;
use crate::journal::Journal;
use crate::data::Dataset;
use crate::schemas::Checks;
use cc_protocol::RpcError;
use cc_protocol::contracts::agents::{ProposeResult, VerifyReport, methods as agents_methods};
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::codes;
use cc_protocol::contracts::memory::{Contract, Task, methods as memory_methods};
use serde_json::{Value, json};


fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

fn run_notes<'a>(journal: &'a Journal, run_id: &str, purpose: &str) -> Vec<&'a JournalEntry> {
    journal
        .entries()
        .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == purpose)
        .collect()
}

fn note_checks(entry: &JournalEntry) -> Vec<String> {
    entry
        .data
        .get("checks")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// A check report as the model reads it.
pub fn report_text(data: &Dataset, heading: &str, report: &VerifyReport) -> String {
    let mut text = data.text(heading, &[]);
    for check in &report.checks {
        let spec = report.contract.checks.iter().find(|spec| spec.name == check.name).map(|spec| spec.spec.as_str()).unwrap_or("");
        let exit = check.exit_code.map(|code| code.to_string()).unwrap_or_else(|| "none".into());
        text.push_str(&data.text("check_output", &[("command", spec), ("exit", &exit), ("output", check.output_tail.trim_end())]));
    }
    text
}

fn describe(data: &Dataset, contract: &Contract) -> String {
    if contract.checks.is_empty() {
        return data.text("checks_none", &[]);
    }
    let mut text = data.text("checks_list", &[]);
    for check in &contract.checks {
        text.push_str(&data.text("decision_check", &[("command", &check.spec)]));
    }
    text
}

/// The CHECKS decision over `current`: a pick among the current checks and the project's other
/// check commands (keep = pick again, replace = pick another, drop = leave out); never free text.
async fn decide(env: &RunEnv, journal: &mut Journal, live: &str, current: &Contract) -> Result<Contract, RpcError> {
    env.set_stage("checks");
    let entries = journal.entries_vec();
    let candidates = crate::verifiers::candidates(env, &crate::flow::run_files(&entries, &env.run_id)).await;
    let mut offered = contract::commands(current);
    for command in crate::verifiers::commands(&candidates) {
        if !offered.contains(&command) {
            offered.push(command);
        }
    }
    let candidates = crate::verifiers::candidates_text(&env.data, &candidates);
    // The spec's open requirements: checks show them where a command can.
    let requirements = crate::spec::live_text(&env.data, &entries, "spec_live");
    let live = [live.to_owned(), requirements, candidates].into_iter().filter(|part| !part.trim().is_empty()).collect::<Vec<_>>().join("\n");
    let live = live.as_str();
    let shape = env.data.shape("CHECKS").cloned().ok_or_else(|| RpcError::internal("the data set has no CHECKS schema"))?;
    let shape = shape.filled(crate::schemas::VERIFIERS_PLACEHOLDER, &offered, None);
    let (decided, _) = decide_json::<Checks>(env, journal, "CHECKS", &shape, live).await?;
    let dirs = [env.scope.workspace.as_str(), env.scope.root.as_str()];
    let commands: Vec<String> = decided.checks.iter().map(|command| contract::rooted(command, &dirs)).collect();
    Ok(contract::contract_from(&commands))
}

async fn record(env: &RunEnv, journal: &mut Journal, contract: &Contract, changed: bool, settled: bool, when: &str) -> Result<(), RpcError> {
    journal
        .append(
            kinds::NOTE,
            json!({
                "text": describe(&env.data, contract),
                "purpose": "checks",
                "run_id": env.run_id,
                "host_message": true,
                "checks": contract::commands(contract),
                "changed": changed,
                "settled": settled,
                "when": when,
            }),
        )
        .await
        .map(|_| ())
}

/// Before the work: baseline runs and CHECKS decisions until the checks stand. Idempotent per run
/// (a resumed run continues from its journal).
pub async fn refine(env: &RunEnv, journal: &mut Journal, proposed: &Contract) -> Result<Contract, RpcError> {
    let notes = run_notes(journal, &env.run_id, "checks");
    if let Some(last) = notes.last()
        && last.data.get("settled").and_then(Value::as_bool).unwrap_or(false)
    {
        return Ok(contract::contract_from(&note_checks(last)));
    }
    let mut history: Vec<Vec<String>> = vec![contract::commands(proposed)];
    history.extend(notes.iter().map(|note| note_checks(note)));
    let mut current = match notes.last() {
        Some(last) => contract::contract_from(&note_checks(last)),
        None => proposed.clone(),
    };
    let mut rounds = notes.len();
    // The checks whose baseline output a decision has seen: every check run before a decision.
    let mut judged: Vec<String> = history[..history.len() - 1].iter().flatten().cloned().collect();
    loop {
        if current.checks.is_empty() {
            record(env, journal, &current, false, true, "before").await?;
            return Ok(current);
        }
        env.set_stage("baseline");
        let any = env.stops.any();
        let (report, _) = contract::verify(&env.host, "baseline", &env.scope.workspace, &current, &any).await?;
        journal
            .append(
                kinds::NOTE,
                json!({
                    "text": report_text(&env.data, "baseline", &report),
                    "purpose": "baseline",
                    "run_id": env.run_id,
                    "host_message": true,
                    "passed": report.checks.iter().filter(|check| check.ok).count(),
                    "checks": report.checks.len(),
                    "failed_tests": failed_tests_by_command(&report),
                }),
            )
            .await?;
        // Structural stop: after a revision, a decision only when a check it has not seen fail fails
        // now. A replacement that passes, or one failing as a check the model kept failed, is settled.
        if rounds > 0 && !unjudged_failure(&report, &judged) {
            record(env, journal, &current, false, true, "before").await?;
            return Ok(current);
        }
        judged.extend(contract::commands(&current));
        let revised = decide(env, journal, "", &current).await?;
        rounds += 1;
        let commands = contract::commands(&revised);
        let changed = commands != contract::commands(&current);
        // Structural stop: kept as is, a revision seen before, or the rounds are spent.
        let settled = !changed || history.contains(&commands) || rounds >= env.data.limits.check_rounds;
        record(env, journal, &revised, changed, settled, "before").await?;
        history.push(commands);
        current = revised;
        if settled {
            return Ok(current);
        }
    }
}

/// Whether a check failed in this report that no decision has seen the output of (by command).
fn unjudged_failure(report: &VerifyReport, judged: &[String]) -> bool {
    report.checks.iter().filter(|check| !check.ok).any(|check| {
        let command = report.contract.checks.iter().find(|spec| spec.name == check.name).map(|spec| spec.spec.as_str()).unwrap_or("");
        !judged.iter().any(|seen| seen == command)
    })
}

/// After the work failed its checks: one CHECKS decision over the failure. Returns the proposed
/// contract when the model replaced a check it judged broken (a proposal, see [`submit`]).
pub async fn propose_after_failure(env: &RunEnv, journal: &mut Journal, current: &Contract, report: &VerifyReport) -> Result<Option<(Contract, String)>, RpcError> {
    let live = report_text(&env.data, "checks_after", report);
    let revised = decide(env, journal, &live, current).await?;
    let changed = contract::commands(&revised) != contract::commands(current);
    if !changed || revised.checks.is_empty() {
        return Ok(None);
    }
    Ok(Some((revised, live)))
}

/// Each failed check's failing tests by its command (only checks whose report names them all).
fn failed_tests_by_command(report: &VerifyReport) -> Value {
    let mut map = serde_json::Map::new();
    for check in report.checks.iter().filter(|check| !check.ok) {
        if let (Some(names), Some(spec)) = (&check.failed_tests, report.contract.checks.iter().find(|spec| spec.name == check.name)) {
            map.insert(spec.spec.clone(), json!(names));
        }
    }
    Value::Object(map)
}

/// The tests a check failed before the work too, out of the ones failing now.
pub struct FailedBefore {
    /// Checks that pass now except for tests that failed before the work too: counted as passed.
    pub settled: Vec<String>,
    /// Failing tests that failed before the work too, in checks that still fail for other tests.
    pub also_before: Vec<String>,
}

/// Compare the failed checks with the run before the work (the latest baseline of this run): a
/// check whose failing tests all failed before the work too is the project's state, not this
/// work's, so it counts as passed (seen: a project's temp-folder tests failing in the sandbox sent
/// the model to rewrite the project's test helper). A check without the names on both sides stays
/// failed.
pub fn failed_before(journal: &Journal, run_id: &str, report: &mut VerifyReport) -> FailedBefore {
    let mut outcome = FailedBefore { settled: Vec::new(), also_before: Vec::new() };
    let Some(before) = run_notes(journal, run_id, "baseline").last().and_then(|note| note.data.get("failed_tests")).and_then(Value::as_object).cloned() else {
        return outcome;
    };
    let specs: Vec<(String, String)> = report.contract.checks.iter().map(|spec| (spec.name.clone(), spec.spec.clone())).collect();
    for check in report.checks.iter_mut().filter(|check| !check.ok && !check.indeterminate) {
        let Some(now) = &check.failed_tests else { continue };
        let Some(spec) = specs.iter().find(|(name, _)| *name == check.name).map(|(_, spec)| spec) else { continue };
        let Some(earlier) = before.get(spec).and_then(Value::as_array) else { continue };
        let earlier: Vec<&str> = earlier.iter().filter_map(Value::as_str).collect();
        let (old, new): (Vec<&String>, Vec<&String>) = now.iter().partition(|name| earlier.contains(&name.as_str()));
        if new.is_empty() {
            check.ok = true;
            outcome.settled.extend(old.into_iter().cloned());
        } else {
            outcome.also_before.extend(old.into_iter().cloned());
        }
    }
    report.ok = report.checks.iter().all(|check| check.ok);
    outcome
}

/// The baseline output a proposal made before the work rests on.
pub fn baseline_evidence(journal: &Journal, run_id: &str) -> String {
    run_notes(journal, run_id, "baseline").last().map(|note| str_field(&note.data, "text").to_owned()).unwrap_or_default()
}

/// Submit a proposed revision of a claimed task's contract to its planner and journal the outcome
/// for the worker. Returns the contract in force afterwards (the accepted revision, or the frozen
/// contract unchanged). Idempotent per run: a resumed run reuses the outcome of the same proposal.
pub async fn submit(
    env: &RunEnv,
    journal: &mut Journal,
    task: &Task,
    proposed: &Contract,
    proposed_by: &str,
    evidence: &str,
    when: &str,
) -> Result<Contract, RpcError> {
    let commands = contract::commands(proposed);
    let earlier = journal
        .entries()
        .filter(|entry| {
            entry.kind == kinds::NOTE
                && str_field(&entry.data, "run_id") == env.run_id
                && str_field(&entry.data, "purpose") == "checks_proposal"
                && str_field(&entry.data, "when") == when
                && note_checks(entry) == commands
        })
        .map(|entry| entry.data.get("accepted").and_then(Value::as_bool).unwrap_or(false))
        .last();
    if let Some(accepted) = earlier {
        if !accepted || task.id.starts_with("local-") {
            return Ok(task.contract.clone());
        }
        let current: Task = env.host.get(memory_methods::TASK_GET, json!({"id": task.id})).await?;
        return Ok(current.contract);
    }
    let outcome = if task.id.starts_with("local-") {
        ProposeResult { accepted: false, reason: env.data.text("checks_no_reviewer", &[]), contract: task.contract.clone(), contract_hash: None }
    } else {
        // A foreground run's review is journaled in its own session; an agent's in its delegator's.
        let session = (task.assignee.is_none()).then(|| env.session_id.clone());
        let params = json!({
            "task_id": task.id,
            "contract": proposed,
            "proposed_by": proposed_by,
            "evidence": evidence,
            "workspace": env.scope.workspace,
            "session_id": session,
        });
        match env.host.call(agents_methods::PROPOSE, params).await {
            Ok(value) => cc_protocol::parse_params(value)?,
            Err(error) if error.code == codes::UNAVAILABLE || error.code == codes::UNKNOWN_METHOD => {
                ProposeResult { accepted: false, reason: env.data.text("checks_no_reviewer", &[]), contract: task.contract.clone(), contract_hash: None }
            }
            Err(error) => return Err(error),
        }
    };
    let mut text = if outcome.accepted {
        env.data.text("checks_accepted", &[("reason", outcome.reason.trim())])
    } else {
        env.data.text("checks_refused", &[("reason", outcome.reason.trim())])
    };
    text.push('\n');
    text.push_str(&describe(&env.data, &outcome.contract));
    journal
        .append(
            kinds::NOTE,
            json!({
                "text": text,
                "purpose": "checks_proposal",
                "run_id": env.run_id,
                "host_message": true,
                "checks": commands,
                "in_force": contract::commands(&outcome.contract),
                "accepted": outcome.accepted,
                "reason": outcome.reason,
                "contract_hash": outcome.contract_hash,
                "when": when,
            }),
        )
        .await?;
    Ok(outcome.contract)
}
