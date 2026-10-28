//! Language guides with the work: a project's language guides (the `always` skills of its language
//! plugins, [`crate::context::Guide`]) are not in the system message, where every call would read
//! them (a greeting's too). The first time a run's work touches a file of a guide's languages (a
//! read, an edit, a host read of a file the task names), the guide comes into the conversation as a
//! host message, right after that file and before the next step: once per session.

use crate::agent_loop::RunEnv;
use crate::journal::Journal;
use cc_protocol::RpcError;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// The purpose of a guide's note.
pub const NOTE_PURPOSE: &str = "guide";

/// Bring the guides of the languages this run touched that the session has not seen yet.
pub async fn deliver(env: &RunEnv, journal: &mut Journal) -> Result<(), RpcError> {
    if env.prefix.guides.is_empty() {
        return Ok(());
    }
    let entries = journal.entries_vec();
    let shown: BTreeSet<&str> =
        entries.iter().filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == NOTE_PURPOSE).map(|entry| str_field(&entry.data, "uri")).collect();
    let mut wanted: Vec<(&str, &str)> = Vec::new();
    for path in touched(&entries, &env.run_id) {
        let name = path.rsplit('/').next().unwrap_or(&path);
        for guide in &env.prefix.guides {
            if shown.contains(guide.uri.as_str()) || wanted.iter().any(|(uri, _)| *uri == guide.uri) {
                continue;
            }
            if let Some(language) = guide.languages.iter().find(|language| language.covers(name)) {
                wanted.push((guide.uri.as_str(), language.name.as_str()));
            }
        }
    }
    for (uri, language) in wanted {
        let Some((category, name)) = uri.strip_prefix("skill://").and_then(|rest| rest.split_once('/')) else { continue };
        let text = match env.host.get::<Value>(&format!("{category}.skill"), json!({"name": name})).await {
            Ok(skill) => crate::context::guide_body(str_field(&skill, "text")).to_owned(),
            Err(error) => {
                tracing::warn!(%error, skill = %uri, "a language guide could not be read");
                continue;
            }
        };
        if text.is_empty() {
            continue;
        }
        let text = env.data.text("language_guide", &[("uri", uri), ("language", language), ("text", &text)]);
        journal.append(kinds::NOTE, json!({"text": text, "purpose": NOTE_PURPOSE, "uri": uri, "run_id": env.run_id, "host_message": true})).await?;
    }
    Ok(())
}

/// The files this run's tool calls named (`path`) and changed, in order.
fn touched(entries: &[JournalEntry], run_id: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for entry in entries.iter().filter(|entry| str_field(&entry.data, "run_id") == run_id) {
        match entry.kind.as_str() {
            kind if kind == kinds::TOOL_CALL => {
                if let Some(path) = entry.data.pointer("/args/path").and_then(Value::as_str) {
                    paths.push(path.to_owned());
                }
            }
            kind if kind == kinds::TOOL_RESULT => {
                paths.extend(entry.data.get("changed_files").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
            }
            _ => {}
        }
    }
    paths
}

fn str_field<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("")
}
