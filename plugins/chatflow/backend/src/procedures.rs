//! Consolidate different real repairs into an owner-reviewed, project-scoped procedure.
use crate::{data::Dataset, host::Host, schemas};
use cc_protocol::{
    RpcError,
    contracts::{
        frame::{JournalEntry, kinds},
        memory::{ProcedureEpisode, ProcedureListResult, ProcedureMaterial, methods},
        model::{ChatMessage, ChatResult, methods as model_methods},
    },
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

fn task_key(work: &str) -> String {
    let normalized = work
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    Sha256::digest(normalized.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn work(entries: &[JournalEntry], run: &str) -> String {
    entries
        .iter()
        .filter(|e| e.data.get("run_id").and_then(|v| v.as_str()) == Some(run))
        .find_map(|e| {
            e.data
                .get("detail")?
                .get("work")?
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
pub async fn episode(
    host: &Host,
    project: &str,
    session: &str,
    run: &str,
    work: &str,
    anchor: &str,
    lesson_id: &str,
    evidence: &str,
    text: &str,
    outcome: &str,
) -> Result<(), RpcError> {
    if work.is_empty() || !anchor.starts_with("lang:") {
        return Ok(());
    }
    let record = ProcedureEpisode {
        project_id: project.into(),
        session_id: session.into(),
        run_id: run.into(),
        task_key: task_key(work),
        anchor: anchor.into(),
        lesson_id: lesson_id.into(),
        evidence: evidence.into(),
        text: text.to_owned(),
        outcome: outcome.into(),
        ..Default::default()
    };
    host.call(
        methods::PROCEDURE_EPISODE,
        serde_json::to_value(record).map_err(|e| RpcError::internal(e.to_string()))?,
    )
    .await
    .map(|_| ())
}

#[derive(Deserialize)]
struct Proposal {
    useful: String,
    name: String,
    applicability: String,
    steps: Vec<String>,
}

/// One call at idle, after the existing learning backlog is drained.
pub async fn synthesize(
    host: &Host,
    data: &Dataset,
    cancel: &CancellationToken,
) -> Result<bool, RpcError> {
    let Some(shape) = data.shape("PROCEDURE") else {
        return Ok(false);
    };
    let mut material: ProcedureMaterial = host.get(methods::PROCEDURE_MATERIAL, json!({})).await?;
    if material.episodes.is_empty() || cancel.is_cancelled() {
        return Ok(false);
    }
    // Outcome history helps the proposer, never the foreground coding model: every revision with
    // every use, so the proposer sees all the advice that failed before.
    for revision in &mut material.history {
        revision.sources.clear();
    }
    let episode_keys: Vec<String> = material.episodes.iter().map(|e| e.key.clone()).collect();
    let mut journals = std::collections::HashMap::new();
    let mut evidence = Vec::new();
    for episode in &material.episodes {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        if !journals.contains_key(&episode.session_id) {
            if let (Ok(session), Ok(entries)) = (
                host.session_get(&episode.session_id).await,
                host.journal_read(&episode.session_id, 0).await,
            ) {
                journals.insert(episode.session_id.clone(), (session.workspace, entries));
            }
        }
        let card = match journals.get(&episode.session_id) {
            Some((workspace, entries)) => {
                crate::learn::procedure_card(
                    host,
                    data,
                    entries,
                    &episode.run_id,
                    workspace,
                    &episode.evidence,
                )
                .await
            }
            None => None,
        };
        evidence.push(json!({"episode": episode, "card": card}));
    }
    // Full episode cards are transient model input, not duplicate logs stored in graph properties.
    let context = json!({"episodes": evidence, "history": material.history});
    let params = json!({
        "messages": [ChatMessage::new("system", data.prompt("procedure")), ChatMessage::new("user", serde_json::to_string(&context).map_err(|e| RpcError::internal(e.to_string()))?)],
        "tools": [], "grammar": schemas::grammar(shape), "sampling": data.sampling("PROCEDURE", false),
        "thinking": crate::llm::thinking("none"), "priority": "background", "idle_only": true,
        "cache_key": "procedure"
    });
    let value = crate::llm::on_best_model(
        host,
        "procedure",
        Some(cancel),
        host.call_with(
            model_methods::CHAT,
            params,
            crate::host::Effect::Idempotent,
            Some(cancel),
        ),
    )
    .await?;
    if cancel.is_cancelled() {
        return Ok(false);
    }
    let result: ChatResult = cc_protocol::parse_params(value)?;
    // An answer the model could not finish proposes nothing.
    if result.finish_reason == "length" {
        return Err(RpcError::internal("The procedure proposal was not finished"));
    }
    let proposal: Proposal = schemas::parse_json(&result.content)
        .map_err(|e| RpcError::internal(format!("Procedure proposal could not be read: {e}")))?;
    host.call(methods::PROCEDURE_PROPOSE, json!({"episode_keys": episode_keys, "useful": proposal.useful == "yes", "name": proposal.name, "applicability": proposal.applicability, "steps": proposal.steps})).await.map(|_| true)
}

/// Only successful journaled exposure is eligible for an eventual owner verdict.
pub async fn outcome(
    host: &Host,
    entries: &[JournalEntry],
    project: &str,
    session: &str,
    run: &str,
    outcome: &str,
) -> Result<(), RpcError> {
    if crate::replay::arm(entries).is_some() {
        return Ok(());
    }
    let revisions: Vec<String> = entries
        .iter()
        .filter(|e| {
            e.kind == kinds::NOTE && e.data.get("run_id").and_then(|v| v.as_str()) == Some(run)
        })
        .filter_map(|e| e.data.get("procedures").and_then(|v| v.as_array()))
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if revisions.is_empty() {
        return Ok(());
    }
    host.call(methods::PROCEDURE_OUTCOME, json!({"project_id": project, "session_id": session, "run_id": run, "outcome": outcome, "revisions": revisions})).await.map(|_| ())
}

pub async fn recall(
    host: &Host,
    project: &str,
    session: &str,
    run: &str,
    work: &str,
    languages: &[String],
) -> ProcedureListResult {
    host.get(methods::PROCEDURE_RECALL, json!({"project_id": project, "session_id": session, "run_id": run, "work": work, "languages": languages})).await.unwrap_or_default()
}
