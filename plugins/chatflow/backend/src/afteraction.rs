//! The after-action record (docs/PROJECT-KNOWLEDGE-PLAN.md P5; docs/LEARNING-GRAPH-IDEAS.md §4).
//!
//! When a run of work ends, the host records it in the graph (`memory.run.record`: outcome, task,
//! the files it changed, and metrics read from the journal: turns, tool calls, steers, whether the
//! checks passed) with no model call. An agent run that carried a contract and did not go clean
//! (checks failed at some point, a steer arrived, or it did not end `done`) then gets one KNOWLEDGE
//! call: what the work showed about the project's world (a cause confirmed or ruled out, how the
//! environment behaves, what a term means), bound only to code the run saw. Those items are written
//! with `source: agent`, half confidence and the run as evidence (`run://<id>`, `TESTED_BY`); they
//! show in the Knowledge section where the user may withdraw them, and never supersede what the
//! user said. A clean run costs nothing but the record.

use crate::agent_loop::{LoopOutcome, RunEnv};
use crate::flow;
use crate::journal::Journal;
use crate::schemas::KnowledgeProposed;
use cc_protocol::RpcError;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::memory::methods as memory_methods;
use cc_protocol::codes;
use serde_json::{Value, json};

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

/// What the journal says about one run, as numbers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metrics {
    pub turns: u32,
    pub tool_calls: u32,
    /// User messages that arrived as steers during the run.
    pub steers: u32,
    /// Contract verifications recorded, and how many of them failed.
    pub verifications: u32,
    pub failed_verifications: u32,
    pub verified: Option<bool>,
}

impl Metrics {
    pub fn of(entries: &[JournalEntry], run_id: &str, outcome: &LoopOutcome) -> Self {
        let run: Vec<&JournalEntry> = entries.iter().filter(|entry| str_field(&entry.data, "run_id") == run_id).collect();
        let steers = run.iter().filter(|entry| entry.kind == kinds::USER_MESSAGE && entry.data.get("from_steer").is_some()).count() as u32;
        let verifications: Vec<&&JournalEntry> = run.iter().filter(|entry| entry.kind == kinds::VERIFICATION).collect();
        // A verification entry is `{task_id, report}`; the report (or the entry) says whether the
        // checks passed.
        let failed_check = |data: &Value| -> bool {
            let report = data.get("report").unwrap_or(&Value::Null);
            [data, report].iter().any(|v| v.get("ok").and_then(Value::as_bool) == Some(false) || v.get("passed").and_then(Value::as_bool) == Some(false))
        };
        let failed = verifications.iter().filter(|entry| failed_check(&entry.data)).count() as u32;
        Metrics { turns: outcome.turns, tool_calls: outcome.tool_calls, steers, verifications: verifications.len() as u32, failed_verifications: failed, verified: outcome.verified }
    }

    /// Whether the run had a signal worth a judgement: a failed check, a steer, or an end other
    /// than `done`.
    pub fn signalled(&self, outcome: &LoopOutcome) -> bool {
        self.failed_verifications > 0 || self.steers > 0 || self.verified == Some(false) || outcome.outcome != "done"
    }

    pub fn json(&self) -> Value {
        json!({
            "turns": self.turns, "tool_calls": self.tool_calls, "steers": self.steers,
            "verifications": self.verifications, "failed_verifications": self.failed_verifications, "verified": self.verified
        })
    }
}

/// Record the run in the graph. Returns the run id, or None when memory refused or is away (the
/// run itself is not affected; a warning says why).
pub async fn record_run(env: &RunEnv, journal: &Journal, task_id: Option<&str>, outcome: &LoopOutcome) -> Option<String> {
    let entries = journal.entries_vec();
    let metrics = Metrics::of(&entries, &env.run_id, outcome);
    let params = json!({
        "project_id": env.scope.project_id, "session_id": env.session_id, "task_id": task_id,
        "outcome": outcome.outcome, "summary": outcome.summary, "touched": outcome.changed_files, "metrics": metrics.json()
    });
    match env.host.call(memory_methods::RUN_RECORD, params).await {
        Ok(value) => value.get("id").and_then(Value::as_str).map(str::to_owned),
        Err(error) => {
            tracing::warn!(%error, run_id = %env.run_id, "run not recorded");
            None
        }
    }
}

/// After a signalled agent run with a contract: one KNOWLEDGE call, its items written as the
/// agent's (half confidence, the run as evidence). A clean run, a run without a contract, or a
/// memory that is away: nothing. Cancellation ends it quietly; any other failure is a warning.
pub async fn learn(env: &RunEnv, journal: &mut Journal, run_ref: Option<&str>, outcome: &LoopOutcome, has_contract: bool) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    let metrics = Metrics::of(&entries, &env.run_id, outcome);
    if !has_contract || !metrics.signalled(outcome) || outcome.outcome == "cancelled" || outcome.outcome == "parked" {
        return Ok(());
    }
    let route_seq = entries.iter().find(|entry| entry.kind == kinds::ROUTE && str_field(&entry.data, "run_id") == env.run_id).map(|entry| entry.seq).unwrap_or(0);
    let binds = flow::turn_binds(&entries, &env.run_id, route_seq, 12);
    let shape = flow::stage_shape(env, "KNOWLEDGE")?.with_reads(&binds, 2);
    let live = env.data.text("afteraction_live", &[("outcome", &outcome.outcome)]);
    let proposed = match flow::decide_json::<KnowledgeProposed>(env, journal, "KNOWLEDGE", &shape, &live).await {
        Ok((proposed, _)) => proposed.items,
        Err(error) if error.code == codes::CANCELLED => return Ok(()),
        Err(error) => {
            tracing::warn!(%error, "the after-action KNOWLEDGE output could not be read");
            return Ok(());
        }
    };
    let mut evidence = vec![format!("journal://{}/{}", env.session_id, route_seq)];
    if let Some(run) = run_ref {
        evidence.insert(0, format!("run://{run}"));
    }
    let mut written = 0;
    for item in proposed {
        if written >= env.data.limits.knowledge_items_per_turn {
            break;
        }
        let kind = item.kind.trim().to_ascii_lowercase();
        let (name, text) = (item.name.trim(), item.text.trim());
        if !matches!(kind.as_str(), "concept" | "fact" | "hypothesis" | "question") || name.is_empty() || text.is_empty() {
            continue;
        }
        let state = match (kind.as_str(), item.state.trim()) {
            ("hypothesis", state @ ("open" | "supported" | "refuted")) => state,
            ("hypothesis", _) | ("question", _) => "open",
            _ => "asserted",
        };
        let rel = if kind == "concept" { "implemented_by" } else { "about" };
        let mut links: Vec<Value> = item.binds.iter().filter(|name| binds.iter().any(|b| b == *name)).map(|name| json!({"rel": rel, "target": name})).collect();
        if let Some(run) = run_ref {
            links.push(json!({"rel": "tested_by", "target": format!("run://{run}")}));
        }
        let params = json!({
            "project_id": env.scope.project_id, "kind": kind, "name": name, "text": text, "state": state, "source": "agent", "confidence": 0.5,
            "by": env.session_id, "evidence": evidence, "links": links
        });
        match env.host.call(memory_methods::KNOWLEDGE_UPSERT, params).await {
            Ok(_) => written += 1,
            Err(error) => {
                tracing::warn!(%error, "after-action item not stored");
                break;
            }
        }
    }
    if written > 0 {
        journal.note(&env.run_id, "afteraction_done", "", false).await?;
    }
    Ok(())
}
