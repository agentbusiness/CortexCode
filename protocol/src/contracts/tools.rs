//! `tools@1`: the model's three tools (read, edit, run), the `cx` long tail, and workspaces.
//!
//! The chat flow forwards the model's raw tool calls to `tools.execute`; argument repair, dialect
//! normalization, hashline anchoring, verification after edits and output offloading all happen
//! here, so every model-facing tool behaviour lives in one swappable plugin.

use crate::contracts::model::{ToolCall, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod methods {
    /// Model-facing tool specs (stable for the life of a session: changing them busts the cache).
    pub const SCHEMAS: &str = "tools.schemas";
    /// Execute one model tool call.
    pub const EXECUTE: &str = "tools.execute";
    /// Host-side direct operations (not via the model).
    pub const READ: &str = "tools.read";
    pub const RUN: &str = "tools.run";
    pub const JOB_STATUS: &str = "tools.job.status";
    pub const JOB_KILL: &str = "tools.job.kill";
    /// Read back offloaded full output (`artifact://<id>`).
    pub const ARTIFACT: &str = "tools.artifact";
    /// Create a copy-on-write workspace clone of a project for an agent.
    pub const WORKSPACE_CREATE: &str = "tools.workspace.create";
    pub const WORKSPACE_REMOVE: &str = "tools.workspace.remove";
    pub const WORKSPACE_LIST: &str = "tools.workspace.list";
    /// Files tree of a workspace (for the UI file browser).
    pub const FILES: &str = "tools.files";
    /// Write a file as the user (UI editor save).
    pub const SAVE: &str = "tools.save";
    /// Normalize (repair) a raw model tool call without executing it.
    pub const NORMALIZE: &str = "tools.normalize";
    /// Background jobs, optionally of one session.
    pub const JOB_LIST: &str = "tools.job.list";
    /// Record that the user approved an ask-mode action class for a session (asked once per class
    /// per session); a call that returned `needs_approval` can then be executed again.
    pub const APPROVE: &str = "tools.approve";
    /// Record skills whose text a session was shown outside the tools (the chat flow's injected
    /// skills), so a diagnostic code that leads to one later brings only its pointer.
    pub const SKILLS_SHOWN: &str = "tools.skills.shown";
    /// A turn ended: `{workspace, session_id, since_ms}` removes what the turn left behind that is
    /// nobody's work (the scratch folder `tmp/`, new untracked files commands made); returns
    /// `{removed: [path]}`.
    pub const CLEANUP: &str = "tools.cleanup";
    /// `{text}`: when the text is a test runner's report with failures, or with tests the runner
    /// could not reach, `{report}` holds its compact form (the counts, one line per failing test with
    /// its error, the first failure whole), else null; `{notes}` holds only what a failure means
    /// where the runner's words hide it and the tests it could not reach (for output shown whole),
    /// else null.
    pub const TESTS_REPORT: &str = "tools.tests.report";
    /// `{session_id, workspace, command}`: runs the test command once on the code as it was before
    /// the session's running turn changed it (each code file `edit` changed put back as it was,
    /// test files left as they are; everything restored after), and returns
    /// `{checked, ran, failed, exit_code, timed_out}`; `checked: false` with `reason` when the turn
    /// changed no code file.
    pub const TESTS_BASELINE: &str = "tools.tests.baseline";
}

/// Approval classes beyond the ask-mode action classes.
pub mod approval_classes {
    /// `write_outside:<absolute dir or file>`: commands of the session may write there although it is
    /// outside the write sandbox (workspace, CORTEX_HOME, temp dirs, package caches, the session's
    /// Python venv). Implied by an ask-mode deletion/overwrite outside the workspace; granted by
    /// `tools.approve`.
    pub const WRITE_OUTSIDE_PREFIX: &str = "write_outside:";
    /// `lift:<command>`: that command runs without the write sandbox (writes anywhere, and the
    /// network, an offline workspace's too). Asked for by the model with a `run`
    /// call's `permission`, in every mode; granted by `tools.approve`.
    pub const LIFT_PREFIX: &str = "lift:";
}

pub mod topics {
    /// `{job_id, session_id, state, exit_code?}` when a background job ends or matches its wake rule.
    pub const JOB_EVENT: &str = "tools.job";
    /// `{workspace, paths}` after an edit changed files.
    pub const FILES_CHANGED: &str = "tools.files_changed";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchemasParams {
    /// Edit format to expose: hashline (default) | replace | whole
    #[serde(default)]
    pub edit_format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchemasResult {
    pub tools: Vec<ToolSpec>,
    /// Contract versions of each tool (tracked in run statistics).
    pub versions: Value,
    /// Model-facing usage notes for the system prompt (kept short and stable).
    pub guide: String,
}

/// Where and for whom a tool call executes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecContext {
    pub project_id: String,
    pub session_id: String,
    /// Absolute path of the directory the call may modify.
    pub workspace: String,
    /// ask | auto
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub edit_format: Option<String>,
}

fn default_mode() -> String {
    "auto".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecuteParams {
    pub context: ExecContext,
    pub call: ToolCall,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolOutcome {
    pub ok: bool,
    /// Text returned to the model as the tool result.
    pub output: String,
    /// Structured data for the UI (diffs, file list, exit code, ...).
    #[serde(default)]
    pub display: Value,
    /// Normalized tool name and arguments after repair (journaled).
    pub tool: String,
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
    /// Full output id when the model-facing output was offloaded.
    #[serde(default)]
    pub artifact: Option<String>,
    /// True when the call needs user approval (output then explains what was asked): in ask mode,
    /// and in every mode when `display.approval.always` is set (a `run` with `permission`).
    #[serde(default)]
    pub needs_approval: bool,
    pub duration_ms: u64,
    /// Structured error class for statistics (stale_anchor, not_found, parse_error, nonzero_exit,
    /// needs_approval, approval_required, protected_path, sandbox_unavailable, ...).
    #[serde(default)]
    pub error_kind: Option<String>,
}

/// Host read (`tools.read`, not via the model): a text file comes back as its exact content (what
/// the UI editor shows and saves), with `total_lines`; a range selects exact lines. Directories,
/// binary files and URIs come back as the model-facing view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReadParams {
    pub workspace: String,
    #[serde(default)]
    pub project_id: Option<String>,
    /// A path relative to the workspace, or a URI (graph://, symbol://, task://, artifact://, ...).
    pub target: String,
    #[serde(default)]
    pub range: Option<String>,
    /// The session the read is made for (a host reading ahead for its model). The result then
    /// carries the skills of the diagnostic codes it shows, once per session like the model's own
    /// `read` (the text the first time, a pointer afterwards). None: no skills.
    #[serde(default)]
    pub session_id: Option<String>,
    /// `diagnostics://`: how long the language servers may take to settle. None: each plugin's
    /// default (the model's reads); 0: what the servers have now, nothing started or waited for
    /// (marked partial when they had not finished).
    #[serde(default)]
    pub wait_ms: Option<u64>,
    /// `diagnostics://`: at most this many listed (errors first), the rest counted. None: all.
    #[serde(default)]
    pub shown: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReadResult {
    pub text: String,
    #[serde(default)]
    pub total_lines: Option<u32>,
}

/// `tools.skills.shown`: these skills' text reached the session (e.g. injected by the chat flow);
/// they count as attached in its skill memory. Answers `{"shown": <how many were new>}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillsShownParams {
    pub session_id: String,
    pub uris: Vec<String>,
}

/// `tools.run` runs in the same write sandbox and Python environment as the `run` tool. A command
/// that is exactly `sandbox-exec -p|-f <profile> [-D k=v]... <command...>` (a caller confining it
/// further, e.g. without network) runs under the intersection of that profile and the write rules;
/// a profile that is not plain allow/deny rules is refused (`invalid_params`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunParams {
    pub workspace: String,
    pub command: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub env: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    pub output: String,
    pub duration_ms: u64,
    pub timed_out: bool,
    #[serde(default)]
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobParams {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobStatus {
    pub job_id: String,
    /// running | exited | killed
    pub state: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub output_tail: String,
    pub started_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactParams {
    pub id: String,
    #[serde(default)]
    pub range: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceCreateParams {
    pub project_id: String,
    /// The project root to clone.
    pub source: String,
    /// Workspace name (agent name).
    pub name: String,
    /// Git branch to create in the clone (default `cortex/<name>`).
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub project_id: String,
    pub path: String,
    pub source: String,
    pub branch: String,
    /// clonefile | copy
    pub method: String,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceRemoveParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceListParams {
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceListResult {
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilesParams {
    pub workspace: String,
    /// Sub-directory relative to the workspace (default root).
    #[serde(default)]
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Excluded by .gitignore / .ignore rules (listed, but the UI may dim or fold it).
    #[serde(default)]
    pub ignored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilesResult {
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SaveParams {
    pub workspace: String,
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SaveResult {
    pub path: String,
    pub bytes: u64,
    /// SHA-256 of the written content.
    pub hash: String,
}

/// Result of `tools.artifact`: the stored bytes, as UTF-8 text when valid, else base64.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactResult {
    pub id: String,
    /// utf8 | base64
    pub encoding: String,
    pub data: String,
    /// Size of the returned slice in bytes.
    pub bytes: u64,
    /// Size of the whole artifact in bytes.
    pub total_bytes: u64,
    pub total_lines: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NormalizeParams {
    pub call: ToolCall,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NormalizeResult {
    pub ok: bool,
    /// read | edit | run (when ok)
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub args: Value,
    /// Repairs that were applied.
    #[serde(default)]
    pub notes: Vec<String>,
    /// `{kind, message, tool?, field?}` when the call cannot be normalized without guessing.
    #[serde(default)]
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct JobListParams {
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobListResult {
    pub jobs: Vec<JobStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApproveParams {
    pub session_id: String,
    /// Action classes from a `needs_approval` outcome (`display.approval.classes`): git_push,
    /// publish, network, delete_outside, overwrite_outside, unresolved, and
    /// `write_outside:<path>` and `lift:<command>` (see [`approval_classes`]).
    pub classes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApproveResult {
    /// Every class approved for the session so far.
    pub approved: Vec<String>,
}
