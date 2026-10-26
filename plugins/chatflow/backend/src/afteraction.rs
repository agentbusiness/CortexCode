//! The after-action record (docs/PROJECT-KNOWLEDGE-PLAN.md P5; docs/LEARNING-GRAPH-IDEAS.md §4).
//!
//! When a run of work ends, the host records it in the graph (`memory.run.record`: outcome, task,
//! the files it changed, and metrics read from the journal: turns, tool calls, steers, whether the
//! checks passed) with no model call. What the run teaches is learned after it ends
//! ([`crate::learn`]).

use crate::agent_loop::{LoopOutcome, RunEnv};
use crate::journal::Journal;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::memory::methods as memory_methods;
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

/// The turn's Changed card: what the session's steps since `since_ms` changed in `workspace`,
/// from the step log (`tools.timeline`), as a transcript-only note `{purpose: "changed"}`. None
/// when nothing changed or the tools plugin is away. Files are the net of the turn's active steps:
/// a file edited then undone in the turn is not listed.
pub async fn changed_note(host: &crate::host::Host, workspace: &str, session_id: &str, since_ms: u64, run_id: &str) -> Option<Value> {
    use cc_protocol::contracts::tools::{TimelineResult, methods as tools_methods};
    let params = json!({"workspace": workspace, "session_id": session_id, "since_ms": since_ms});
    let timeline: TimelineResult = serde_json::from_value(host.call(tools_methods::TIMELINE, params).await.ok()?).ok()?;
    let active: Vec<&cc_protocol::contracts::tools::TimelineRecord> = timeline.records.iter().filter(|record| !record.undone && record.undoes.is_empty()).collect();
    if active.is_empty() {
        return None;
    }
    // Oldest first: a file's first `before` and last `after` make its net change.
    #[derive(Default)]
    struct Net {
        before: Option<String>,
        after: Option<String>,
        added: u32,
        removed: u32,
        binary: bool,
    }
    let mut files: std::collections::BTreeMap<String, Net> = std::collections::BTreeMap::new();
    for record in active.iter().rev() {
        for file in &record.files {
            let entry = files.entry(file.path.clone()).or_insert_with(|| Net { before: file.before.clone(), binary: file.binary, ..Net::default() });
            entry.after = file.after.clone();
            entry.added += file.added;
            entry.removed += file.removed;
        }
    }
    let seqs: Vec<u64> = active.iter().map(|record| record.seq).collect();
    let listed: Vec<Value> = files
        .into_iter()
        .filter(|(_, net)| net.before != net.after)
        .map(|(path, net)| {
            json!({"path": path, "added": net.added, "removed": net.removed, "created": net.before.is_none(), "deleted": net.after.is_none(), "binary": net.binary})
        })
        .collect();
    if listed.is_empty() {
        return None;
    }
    Some(json!({
        "purpose": "changed", "text": "", "run_id": run_id, "workspace": workspace,
        "files": listed, "seqs": seqs, "from_seq": seqs.iter().min(), "to_seq": seqs.iter().max(),
    }))
}
