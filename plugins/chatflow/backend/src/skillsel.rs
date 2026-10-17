//! Skill selection (a measured mechanism, placed on CortexCode's stages).
//!
//! - The turn's first generation chooses (fix 1): ROUTE writes `work`, then `route`, then
//!   `skills` on the routes that work (ANSWER, LOOKUP, DO, DELEGATE, PLAN; `router.json` `x-when`),
//!   so each summary is compared with a statement of the work, not with a solution worked out
//!   later (DECIDE's `brief`, written after exploring, named the fix and matched nothing). STATUS
//!   chooses none; IMPLEMENT runs each plan task with the skills that task chose.
//! - The field is an enum of the session's catalog (`Prefix::skills`), capped by
//!   `limits.skills_per_turn` (0 removes it), so the model can only name skills that exist. The
//!   catalog's lines (uri and summary) are shown in the choosing stage's message, never in the
//!   prefix (it stays byte-stable, and the other calls do not carry them).
//! - The skills a previous turn used come first in the enum and in those lines, and are named in
//!   the stage message ("used in the previous turn").
//! - The chosen skills reach the turn once, right after ROUTE and before anything else runs (spec,
//!   reads, DECIDE, the tool loop, REPLY, PLAN), as a host note: the first in full, the others as
//!   their catalog line to read when that part comes. Host notes are never folded. A DECIDE that
//!   changes the route keeps them; DELEGATE passes them to the agents.
//! - `read skill://...` stays available in the loop for symptoms that appear mid-work.

use crate::agent_loop::RunEnv;
use crate::journal::Journal;
use cc_protocol::RpcError;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::tools::methods as tools_methods;
use serde_json::{Value, json};

/// Purpose of the note that carries the chosen skills into a run.
pub const NOTE_PURPOSE: &str = "skills";

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

fn uris(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// The skill a `read` call fetched, when it was a skill read.
pub fn read_skill_uri(call: &JournalEntry) -> Option<String> {
    if call.kind != kinds::TOOL_CALL || str_field(&call.data, "tool") != "read" {
        return None;
    }
    let path = call.data.get("args").and_then(|args| args.get("path")).and_then(Value::as_str)?;
    path.starts_with("skill://").then(|| path.to_owned())
}

/// The skills one run used: chosen by its decisions and read by its loop, in first-use order.
pub fn used_by_run(entries: &[JournalEntry], run_id: &str) -> Vec<String> {
    let mut used: Vec<String> = Vec::new();
    let mut add = |uri: String| {
        if !used.contains(&uri) {
            used.push(uri);
        }
    };
    for entry in entries.iter().filter(|entry| str_field(&entry.data, "run_id") == run_id) {
        match entry.kind.as_str() {
            kind if kind == kinds::ROUTE => of_decision(entry).into_iter().for_each(&mut add),
            kind if kind == kinds::NOTE && str_field(&entry.data, "purpose") == NOTE_PURPOSE => uris(entry.data.get("skills")).into_iter().for_each(&mut add),
            _ => {
                if let Some(uri) = read_skill_uri(entry) {
                    add(uri);
                }
            }
        }
    }
    used
}

/// The skills the turn before this run used (the newest earlier run of the session).
pub fn previous_turn(entries: &[JournalEntry], run_id: &str) -> Vec<String> {
    let first_of_run = entries.iter().position(|entry| str_field(&entry.data, "run_id") == run_id).unwrap_or(entries.len());
    let previous = entries[..first_of_run].iter().rev().map(|entry| str_field(&entry.data, "run_id")).find(|id| !id.is_empty() && *id != run_id);
    previous.map(|previous| used_by_run(entries, previous)).unwrap_or_default()
}

/// The catalog in the order the decision sees it: the previous turn's skills first.
pub fn ordered_catalog(catalog: &[String], previous: &[String]) -> Vec<String> {
    let mut ordered: Vec<String> = previous.iter().filter(|uri| catalog.contains(uri)).cloned().collect();
    let rest: Vec<String> = catalog.iter().filter(|uri| !ordered.contains(uri)).cloned().collect();
    ordered.extend(rest);
    ordered
}

/// The skills a decision entry (`route` kind) carries in its `detail`.
pub fn of_decision(entry: &JournalEntry) -> Vec<String> {
    uris(entry.data.get("detail").and_then(|detail| detail.get("skills")))
}

/// Whether the skills of the decision at `decision_seq` are in the conversation already.
pub fn injected<'a>(entries: impl IntoIterator<Item = &'a JournalEntry>, decision_seq: u64) -> bool {
    entries.into_iter().any(|entry| {
        entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == NOTE_PURPOSE && entry.data.get("decision_seq").and_then(Value::as_u64) == Some(decision_seq)
    })
}

/// The decision's choice, kept to the catalog, without repeats, at most `max`.
pub fn chosen(skills: &[String], catalog: &[String], max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for uri in skills {
        if catalog.contains(uri) && !out.contains(uri) && out.len() < max {
            out.push(uri.clone());
        }
    }
    out
}

/// `skill://lang-rust/rust-errors` -> ("lang-rust", "rust-errors").
fn split_uri(uri: &str) -> Option<(&str, &str)> {
    uri.strip_prefix("skill://")?.split_once('/')
}

/// The chosen skills into the run, once per decision: a host note with the first skill's text and
/// the others' catalog lines. Nothing is appended when nothing was chosen or the note exists.
pub async fn inject(env: &RunEnv, journal: &mut Journal, decision_seq: u64, skills: &[String]) -> Result<(), RpcError> {
    if skills.is_empty() {
        return Ok(());
    }
    if injected(journal.entries(), decision_seq) {
        return Ok(());
    }
    let (first, rest) = skills.split_first().expect("not empty");
    let mut inlined = false;
    let body = match split_uri(first) {
        Some((category, name)) => match env.host.get::<Value>(&format!("{category}.skill"), json!({"name": name})).await {
            Ok(skill) => {
                inlined = true;
                str_field(&skill, "text").to_owned()
            }
            Err(error) => env.data.text("skill_unavailable", &[("uri", first), ("error", &error.message)]),
        },
        None => String::new(),
    };
    let mut text = env.data.text("skills_chosen", &[("uri", first), ("text", body.trim())]);
    if !rest.is_empty() {
        // The others as their catalog lines: the summary says when to read each.
        let lines: Vec<String> = rest.iter().map(|uri| env.prefix.skill_line(uri)).collect();
        text.push_str(&env.data.text("skills_also", &[("uris", &lines.join("\n"))]));
    }
    journal
        .append(
            kinds::NOTE,
            json!({"text": text, "purpose": NOTE_PURPOSE, "skills": skills, "decision_seq": decision_seq, "run_id": env.run_id, "host_message": true}),
        )
        .await?;
    // The inlined skill counts as shown in the tools' per-session memory: a diagnostic code that
    // leads to it later brings only its pointer. Once, not retried (a miss only repeats the text).
    if let Some(params) = shown_params(&env.session_id, skills, inlined) {
        let _ = env.host.ctx().call(tools_methods::SKILLS_SHOWN, params).await;
    }
    Ok(())
}

/// `tools.skills.shown` for the skill whose text the note carries (the first, when its text was
/// fetched); the others are catalog lines only, so a code may still bring their text.
fn shown_params(session_id: &str, skills: &[String], inlined: bool) -> Option<Value> {
    let first = skills.first().filter(|_| inlined)?;
    Some(json!({"session_id": session_id, "uris": [first]}))
}
