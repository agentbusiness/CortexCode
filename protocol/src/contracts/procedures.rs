//! Project-scoped learned procedures. A finished run is an observation, not proof of benefit.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcedureEpisode {
    #[serde(default)]
    pub key: String,
    pub project_id: String,
    pub session_id: String,
    pub run_id: String,
    pub task_key: String,
    pub anchor: String,
    pub lesson_id: String,
    #[serde(default)]
    pub evidence: String,
    pub text: String,
    pub outcome: String,
    #[serde(default)]
    pub created_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcedureRevision {
    pub key: String,
    pub family: String,
    pub project_id: String,
    pub anchor: String,
    pub name: String,
    pub applicability: String,
    pub steps: Vec<String>,
    pub state: String,
    #[serde(default)]
    pub ready: bool,
    pub created_ms: u64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub sources: Vec<ProcedureEpisode>,
    #[serde(default)]
    pub uses: Vec<ProcedureUse>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcedureUse {
    pub key: String,
    pub revision: String,
    pub project_id: String,
    pub session_id: String,
    pub run_id: String,
    pub task_key: String,
    pub work: String,
    pub outcome: String,
    #[serde(default)]
    pub shown: bool,
    #[serde(default)]
    pub verdict: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub created_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcedureMaterial {
    pub episodes: Vec<ProcedureEpisode>,
    pub history: Vec<ProcedureRevision>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcedureListParams {
    pub project_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcedureListResult {
    pub items: Vec<ProcedureRevision>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcedureProposal {
    pub episode_keys: Vec<String>,
    pub name: String,
    pub applicability: String,
    pub steps: Vec<String>,
    /// False records an unsuccessful synthesis attempt, without an inactive pseudo-procedure.
    pub useful: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcedureRecallParams {
    pub project_id: String,
    pub work: String,
    pub languages: Vec<String>,
    pub session_id: String,
    pub run_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcedureOutcome {
    pub project_id: String,
    pub session_id: String,
    pub run_id: String,
    pub outcome: String,
    pub revisions: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcedureReview {
    pub project_id: String,
    pub revision: String,
    /// trial, approve, reject, withdraw, helped, hurt, unclear.
    pub action: String,
    #[serde(default)]
    pub use_key: Option<String>,
    #[serde(default)]
    pub reason: String,
}
