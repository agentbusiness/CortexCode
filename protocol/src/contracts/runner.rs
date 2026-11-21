//! `runner@1`: a CortexCode that builds for a manager (docs/specs/01-WORK.md). CortexCode's own
//! contract; CortexManage carries this file to speak it through `mesh.link`. One place for
//! everything a manager needs: prepare a workspace, take a finished plan and start it, report
//! facts, restart a task from a checkpoint, build into the proving home, land.
//!
//! A job is `{job_id, attempt}`. The runner acts only on the current attempt: a late call from a
//! replaced attempt is refused with `conflict`.

use serde::{Deserialize, Serialize};

pub mod methods {
    /// [`super::PrepareParams`] -> [`super::Prepared`]: fetch the mirror, add the package's
    /// worktree at `base`, open it as a project.
    pub const PREPARE: &str = "runner.prepare";
    /// [`super::StartParams`] -> [`super::Started`]: put the plan in as approved and implement it.
    pub const START: &str = "runner.start";
    /// `{job_id}` -> [`super::Facts`].
    pub const GET: &str = "runner.get";
    /// `{}` -> `{jobs: [Facts], repos: [url]}`: its jobs, and the sources it holds a mirror of.
    pub const LIST: &str = "runner.list";
    /// `{job_id, attempt}` -> [`super::Facts`]: cancel the run; the worktree stays.
    pub const STOP: &str = "runner.stop";
    /// [`super::RestartParams`] -> [`super::Started`]: restore the last checkpoint and start the
    /// task again in a fresh session.
    pub const RESTART: &str = "runner.restart";
    /// [`super::SteerParams`] -> `{ok}`: a fixed sentence to the builder, delivered at its next
    /// tool boundary. Refused while the session waits on an approval.
    pub const STEER: &str = "runner.steer";
    /// [`super::ReplanAnswer`] -> `{ok}`: answer a held replan.
    pub const REPLAN_ANSWER: &str = "runner.replan.answer";
    /// [`super::ApprovalAnswer`] -> `{ok}`: answer the builder's approval question.
    pub const APPROVAL_ANSWER: &str = "runner.approval.answer";
    /// `{job_id}` -> [`super::DiffAnswer`]: the diff since the last checkpoint (or since `base`
    /// with `whole: true`).
    pub const DIFF: &str = "runner.diff";
    /// [`super::BuildParams`] -> [`super::BuildAnswer`]: build the changed parts into the proving home.
    pub const BUILD: &str = "runner.build";
    /// [`super::CommandParams`] -> [`super::CommandAnswer`]: run a walk's command in the proving home.
    pub const COMMAND: &str = "runner.command";
    /// [`super::LandParams`] -> [`super::Landed`]: fetch, rebase, squash per request, fast-forward
    /// the main branch, push.
    pub const LAND: &str = "runner.land";
    /// `{job_id}` -> `{ok}`: remove the worktree.
    pub const REMOVE: &str = "runner.remove";
    /// [`super::BeatAnswer`] -> `{ok}`: the manager's answer to a beat (`continue` or `stop`).
    pub const BEAT_ANSWER: &str = "runner.beat.answer";
    /// [`super::ReadParams`] -> `{text}`: read a file of the mirror at the main branch (planning's
    /// read-only discovery).
    pub const READ: &str = "runner.read";
    /// [`super::SearchParams`] -> `{hits: [{path, line, text}]}`: search the mirror's main branch.
    pub const SEARCH: &str = "runner.search";
    /// `{repo, path?}` -> `{entries: [string]}`: list a folder of the mirror's main branch.
    pub const TREE: &str = "runner.tree";
    /// [`super::ReleaseParams`] -> [`super::ReleaseAnswer`]: run the repository's own release
    /// scripts in a clean checkout of the main branch (`plan_only`: only say what would be released).
    pub const RELEASE: &str = "runner.release";
}

pub mod topics {
    /// A [`super::Facts`], every 20 s while a job runs and at once on a change of task, a
    /// checkpoint, a held replan or an approval question.
    pub const BEAT: &str = "runner.beat";
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PrepareParams {
    pub job_id: String,
    /// The repository's short id (`cortexterm`): the folder under the runner's work folder.
    pub repo: String,
    /// The git remote of the main line.
    pub url: String,
    /// The main branch.
    pub main: String,
    /// `cm/<package>-<slug>`.
    pub branch: String,
    /// The commit to start from; empty: the main branch's head after the fetch.
    #[serde(default)]
    pub base: String,
    /// Who the job's commits are made as (author and committer). Both empty: git's own identity
    /// on this computer.
    #[serde(default)]
    pub commit_name: String,
    #[serde(default)]
    pub commit_email: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Prepared {
    pub project_id: String,
    pub root: String,
    /// The commit the worktree starts from.
    pub base: String,
}

/// One task as the builder gets it. No walks: those stay with the manager.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub n: u32,
    pub title: String,
    pub brief: String,
    #[serde(default)]
    pub known: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<u32>,
    #[serde(default)]
    pub touches: Vec<String>,
    #[serde(default)]
    pub checks: Vec<String>,
    /// The request it belongs to (for the commit's trailer and the squash).
    #[serde(default)]
    pub request: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StartParams {
    pub job_id: String,
    pub attempt: u32,
    pub project_id: String,
    pub title: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub approach: String,
    pub tasks: Vec<Task>,
    /// Paths no task may change (the standard's files, `AGENTS.md`), unless `standard_change`.
    #[serde(default)]
    pub protected: Vec<String>,
    #[serde(default)]
    pub standard_change: bool,
    /// Approval classes the plan lists; the runner refuses every other approval question at once,
    /// so a job never waits for an answer.
    #[serde(default)]
    pub approve: Vec<String>,
    /// Lines for every commit's trailer (`Package: pk-0142`, `Charter: …`).
    #[serde(default)]
    pub trailers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Started {
    pub session_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RestartParams {
    pub job_id: String,
    pub attempt: u32,
    /// The task to start again.
    pub task: u32,
    /// What was tried and what stopped it: one line added to the task's brief.
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SteerParams {
    pub job_id: String,
    pub attempt: u32,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReplanAnswer {
    pub job_id: String,
    pub attempt: u32,
    pub accept: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ApprovalAnswer {
    pub job_id: String,
    pub attempt: u32,
    pub seq: u64,
    pub approve: bool,
    /// On a refusal: the sentence saying what to do instead.
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BeatAnswer {
    pub job_id: String,
    pub attempt: u32,
    /// `continue` | `stop`.
    pub want: String,
}

/// A checkpoint: the host's commit after a task whose check passed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Checkpoint {
    pub task: u32,
    pub commit: String,
    pub ms: u64,
    pub files: u32,
    pub lines: u32,
}

/// A replan CortexCode wants, held until the manager answers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HeldReplan {
    /// What the work showed.
    pub found: String,
    /// The tasks still to do, as CortexCode would rewrite them.
    pub tasks: Vec<Task>,
}

/// The builder's approval question.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HeldApproval {
    pub seq: u64,
    pub tool: String,
    pub classes: Vec<String>,
    pub question: String,
}

/// The facts of a job, computed by the runner's code. No judgement in here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Facts {
    pub job_id: String,
    pub attempt: u32,
    /// waiting | running | done | failed | stopped | refused | lost.
    pub state: String,
    /// Why it stopped, when it did: done | cancelled | error | context | approval | replan | lane.
    #[serde(default)]
    pub stopped_because: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub project_id: String,
    /// The task in work (1-based), and how many there are.
    #[serde(default)]
    pub task: u32,
    #[serde(default)]
    pub tasks: u32,
    #[serde(default)]
    pub stage: String,
    /// The last tool call in plain words (`editing desk/focus.ts`).
    #[serde(default)]
    pub now: String,
    /// Successful calls that were not edits since the last edit.
    #[serde(default)]
    pub calls_since_edit: u32,
    #[serde(default)]
    pub idle_ms: u64,
    /// The longest run of the same call with the same result.
    #[serde(default)]
    pub same_call: u32,
    /// The longest run of the same failing command, and the command.
    #[serde(default)]
    pub same_failure: u32,
    #[serde(default)]
    pub failing_command: String,
    /// Changed since the last checkpoint.
    #[serde(default)]
    pub files: u32,
    #[serde(default)]
    pub lines: u32,
    /// Changed since the base.
    #[serde(default)]
    pub total_files: u32,
    #[serde(default)]
    pub total_lines: u32,
    /// Files changed that are in no task's `touches`.
    #[serde(default)]
    pub out_of_scope: Vec<String>,
    /// Protected files changed.
    #[serde(default)]
    pub protected_changed: Vec<String>,
    #[serde(default)]
    pub minutes: u64,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub checkpoints: Vec<Checkpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replan: Option<HeldReplan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<HeldApproval>,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiffAnswer {
    /// Unified diff, cut at `limit` bytes (default 60000) at a file boundary.
    pub diff: String,
    pub files: Vec<String>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BuildParams {
    pub job_id: String,
    pub attempt: u32,
    /// The app's id (`cortexterm`): the proving home is `~/.cortex-proving/<app>`.
    pub app: String,
    /// The parts to build (plugin ids, `frame`); empty: every part whose files changed.
    #[serde(default)]
    pub parts: Vec<String>,
    /// Install into the app's own home (the soak) instead of the proving home.
    #[serde(default)]
    pub soak: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BuildAnswer {
    pub ok: bool,
    pub parts: Vec<String>,
    /// The last lines of the build's output.
    #[serde(default)]
    pub tail: Vec<String>,
    /// The commit that was built.
    #[serde(default)]
    pub build: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CommandParams {
    pub job_id: String,
    pub app: String,
    pub command: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CommandAnswer {
    pub exit_code: i32,
    pub output: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LandParams {
    pub job_id: String,
    pub attempt: u32,
    /// One commit per request: `[request id, title]`, in order.
    pub requests: Vec<(String, String)>,
    /// `none` | `github-pr`.
    #[serde(default)]
    pub changes: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Landed {
    pub ok: bool,
    /// The main branch's head after landing.
    #[serde(default)]
    pub commit: String,
    /// True when the main branch had moved and the branch was rebased: build and prove again
    /// before calling land a second time with `rebased_ok`.
    #[serde(default)]
    pub rebased: bool,
    /// Files in conflict, when the rebase stopped.
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReadParams {
    pub repo: String,
    pub path: String,
    /// 1-based first line and how many lines (default: the first 400).
    #[serde(default)]
    pub from: Option<u32>,
    #[serde(default)]
    pub lines: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchParams {
    pub repo: String,
    /// A fixed string (not a pattern).
    pub text: String,
    /// A path prefix to search under.
    #[serde(default)]
    pub under: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReleaseParams {
    pub repo: String,
    pub url: String,
    pub main: String,
    #[serde(default)]
    pub plan_only: bool,
}

/// One part a release moves.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReleasedPart {
    pub part: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReleaseAnswer {
    pub ok: bool,
    pub parts: Vec<ReleasedPart>,
    /// The last lines of the script's output.
    #[serde(default)]
    pub tail: Vec<String>,
}
