//! The project's own check commands, as the language plugins declare them (`lang@1` describe's
//! `verifiers`): what DECIDE and CHECKS see as candidates for `checks`, so a decision picks a real
//! build or test command of this project instead of inventing one. A verifier applies when one of
//! its marker files (`when`) exists at the project root (no markers: it always applies) and every
//! program it `requires` is found on the PATH its command runs with: the process PATH with the
//! language plugins' toolchain directories (`Description.path`) in front. A lookup only, no process
//! is started. The candidates are what a decision's `checks` can be (a pick, never free text): the
//! host fills `{file}` / `{files}` with the files the turn's decisions named and leaves a verifier
//! out when it needs files and none were named. A project with no applicable verifier gets no
//! checks: the host then runs nothing and the work is verified by the model's own runs (owner,
//! 2026-10-16: no check may carry an expected value the model worked out).

use crate::agent_loop::RunEnv;
use cc_protocol::contracts::lang::{CONTRACT, Description};
use serde_json::{Value, json};
use std::path::Path;

// The applicability and filling rules are shared with the agents planner (`lang::verifiers`).
pub use cc_protocol::contracts::lang::verifiers::{Candidate, commands};

/// The project's candidate checks: the applicable verifiers of the active language plugins, filled
/// with `files` (the files the turn's decisions named); empty when nothing applies.
pub async fn candidates(env: &RunEnv, files: &[String]) -> Vec<Candidate> {
    let Ok(listed) = env.host.get::<Value>("frame.plugins.list", json!({})).await else { return Vec::new() };
    let mut descriptions: Vec<Description> = Vec::new();
    for entry in listed.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default() {
        let field = |name: &str| entry.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        if field("contract") != CONTRACT || field("state") != "active" {
            continue;
        }
        if let Ok(description) = env.host.get::<Description>(&format!("{}.describe", field("category")), json!({})).await {
            descriptions.push(description);
        }
    }
    cc_protocol::contracts::lang::verifiers::candidates(Path::new(&env.scope.root), &descriptions, std::env::var_os("PATH").as_deref(), files)
}

/// The candidates as the stage message's text; empty when nothing applies.
pub fn candidates_text(data: &crate::data::Dataset, candidates: &[Candidate]) -> String {
    if candidates.is_empty() {
        return String::new();
    }
    let list = candidates.iter().map(|candidate| data.text("verifier_line", &[("kind", &candidate.kind), ("command", &candidate.command)])).collect::<Vec<_>>().join("\n");
    data.text("verifiers_candidates", &[("list", &list)])
}
