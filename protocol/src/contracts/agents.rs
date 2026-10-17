//! `agents@1`: named background agents, the scheduler, task delegation, contract verification and
//! the merge steward. Agent sessions, tasks and board messages are journal / graph state, so this
//! plugin can be hot-swapped mid-work; running agents are re-adopted from their records.

use crate::contracts::memory::{AgentRecord, Contract, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod methods {
    /// Turn a goal into a task tree (planned by the model over the code graph) and start agents.
    pub const DELEGATE: &str = "agents.delegate";
    /// Start a named agent on an existing task.
    pub const SPAWN: &str = "agents.spawn";
    pub const LIST: &str = "agents.list";
    pub const STEER: &str = "agents.steer";
    pub const STOP: &str = "agents.stop";
    /// Run a task's contract checks in its workspace and record the verdict.
    pub const VERIFY: &str = "agents.verify";
    /// Merge a verified agent workspace branch into the project.
    pub const MERGE: &str = "agents.merge";
    /// Scheduler state: running, queued, slots.
    pub const SCHEDULER: &str = "agents.scheduler";
    /// [`super::ProposeParams`] -> [`super::ProposeResult`]: a task's worker proposes a revision of
    /// its frozen contract; the planner (a fresh context, never the worker) accepts or refuses it,
    /// and an accepted revision is recorded with the old hash and the reason.
    pub const PROPOSE: &str = "agents.contract.propose";
}

pub mod topics {
    /// `{agent, project_id, status, task_id}`
    pub const AGENT_CHANGED: &str = "agents.changed";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DelegateParams {
    pub project_id: String,
    /// Session that delegated (receives agent_spawned / verification entries).
    pub from_session: String,
    pub goal: String,
    /// Context the conductor already gathered (files, symbols, constraints).
    #[serde(default)]
    pub context: String,
    /// Maximum agents to run in parallel for this goal (default: what the scheduler allows).
    #[serde(default)]
    pub max_agents: Option<u32>,
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "auto".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DelegateResult {
    pub root_task: Task,
    pub tasks: Vec<Task>,
    pub agents: Vec<AgentRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpawnParams {
    pub project_id: String,
    pub task_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub from_session: Option<String>,
    #[serde(default = "default_mode")]
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListParams {
    pub project_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListResult {
    pub agents: Vec<AgentRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentSteerParams {
    pub project_id: String,
    pub agent: String,
    pub text: String,
    #[serde(default = "default_steer")]
    pub kind: String,
    /// Made by `chatflow.attach`; forwarded to the agent's session with `chatflow.steer`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<super::chatflow::Attachment>,
    /// Other projects the message references; forwarded with `chatflow.steer`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<super::chatflow::Mention>,
}

fn default_steer() -> String {
    "steer".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentRefParams {
    pub project_id: String,
    pub agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifyParams {
    pub task_id: String,
    /// Workspace to verify in (default: the assignee's workspace, else the project root).
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckResult {
    pub name: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub output_tail: String,
    pub duration_ms: u64,
    /// The host could not decide the check (a manual or unknown kind): never `ok`, and not a
    /// failure the worker can fix either.
    #[serde(default)]
    pub indeterminate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifyReport {
    pub task_id: String,
    pub ok: bool,
    pub checks: Vec<CheckResult>,
    pub contract: Contract,
    pub ts_ms: u64,
    /// Hash of the contract that was run.
    #[serde(default)]
    pub contract_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposeParams {
    pub task_id: String,
    pub contract: Contract,
    /// The worker: agent name, or `session:<id>` for a foreground run.
    pub proposed_by: String,
    /// What the proposal rests on: the checks' outputs where they ran.
    #[serde(default)]
    pub evidence: String,
    /// The workspace the evidence comes from (checks are judged against it and the project root).
    #[serde(default)]
    pub workspace: Option<String>,
    /// Session whose journal records the review (default: the task's delegating session).
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposeResult {
    pub accepted: bool,
    pub reason: String,
    /// The contract in force afterwards, and its hash.
    pub contract: Contract,
    #[serde(default)]
    pub contract_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MergeParams {
    pub project_id: String,
    pub agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MergeOutcome {
    pub ok: bool,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub sha: Option<String>,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchedulerState {
    pub running: Vec<String>,
    pub queued: Vec<String>,
    pub max_parallel: u32,
    #[serde(default)]
    pub detail: Value,
}
