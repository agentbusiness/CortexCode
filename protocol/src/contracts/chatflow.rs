//! `chatflow@1`: the conversation and agent loop (router -> host reads -> decide -> act/reply).
//!
//! All progress is written to the session journal through the frame; these methods return quickly
//! (except `chatflow.agent_run`, which runs an agent loop to completion for the agents plugin).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod methods {
    /// Start a foreground turn from a user message. Returns immediately with the run id.
    pub const TURN: &str = "chatflow.turn";
    /// Steer a running session (`steer`: inject at the next tool boundary; `followup`: after the run).
    pub const STEER: &str = "chatflow.steer";
    pub const CANCEL: &str = "chatflow.cancel";
    pub const STATE: &str = "chatflow.state";
    /// Run an agent loop on a task in a session to completion (streams progress items).
    pub const AGENT_RUN: &str = "chatflow.agent_run";
    /// Answer a pending approval (ask mode): [`super::ApproveParams`] -> `{state}`.
    pub const APPROVE: &str = "chatflow.approve";
    /// Put a message attachment into the content-addressed attachment store:
    /// [`super::AttachmentPutParams`] -> [`super::AttachmentPutResult`]. Bytes (a pasted screenshot
    /// the shell wrote to `$CORTEX_HOME/attachments/incoming/`, or base64 `data` where there is no
    /// shell) are stored as `$CORTEX_HOME/attachments/<sha256>.<ext>`; a file or folder of the
    /// project is referenced by its path (nothing is copied, nothing is written into the
    /// repository); a file from elsewhere is copied into the store. The shape is the planned
    /// `frame.attachment.put`'s, so the store can move to the frame without reshaping callers.
    pub const ATTACHMENT_PUT: &str = "chatflow.attachment.put";
    /// The user's edit of the session's spec: [`super::SpecUpdateParams`] -> `{version}` (a new
    /// `spec` entry; requirements without an id get the next free one).
    pub const SPEC_UPDATE: &str = "chatflow.spec.update";
    /// Write the current spec into the repository as `docs/specs/<slug>.md` (the session's
    /// workspace): [`super::SessionParams`] -> `{path}`.
    pub const SPEC_SAVE: &str = "chatflow.spec.save";
    /// Approve a draft plan or return an approved one to draft: [`super::PlanUpdateParams`] ->
    /// `{status, changed}`.
    pub const PLAN_UPDATE: &str = "chatflow.plan.update";
    /// Hand the session's open plan to the task and agent machinery (a turn whose route is
    /// IMPLEMENT): [`super::PlanRefParams`] -> [`super::TurnResult`].
    pub const PLAN_IMPLEMENT: &str = "chatflow.plan.implement";
    /// Accept or reject a proposed decision: [`super::DecisionAnswerParams`] -> `{state, changed}`.
    /// An accepted decision joins the spec's `decisions` (or the session's decision list) and the
    /// project's knowledge (`memory.knowledge.upsert`, kind `decision`; the entry keeps the
    /// `knowledge_id`).
    pub const DECISION_ANSWER: &str = "chatflow.decision.answer";
    /// Accept (with the text as edited), or reject, a knowledge item the model proposed from the
    /// user's message: [`super::KnowledgeAnswerParams`] -> `{state, changed, stored?}`. An
    /// accepted item is written to the project's knowledge (docs/PROJECT-KNOWLEDGE-PLAN.md P3).
    pub const KNOWLEDGE_ANSWER: &str = "chatflow.knowledge.answer";
    /// Prefill the session's prompt prefix into the model's cache in the background (a conversation
    /// opened or selected, the user typing, a turn ended), so the next turn's first call reads only
    /// the new message: [`super::PrewarmParams`] -> [`super::PrewarmResult`]. Best effort and at
    /// background priority; the session's next model call takes over what it prefilled. With a
    /// `project_id` and no session (a new conversation being written), the project's chat system
    /// block is prefilled for the conversation about to be created.
    pub const PREWARM: &str = "chatflow.prewarm";
}

/// What `chatflow.prewarm` warms: a session, or the system block of a project's next conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PrewarmParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// What `chatflow.prewarm` did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PrewarmResult {
    /// started (a prefill was sent) | warm (this session state was prefilled already or is being
    /// prefilled) | running (a run is active: its own calls keep the cache) | skipped (nothing to
    /// warm: no model size known, or the session is not a chat).
    pub state: String,
    /// Messages of the prefix sent (started / warm).
    #[serde(default)]
    pub messages: u32,
}

/// Limits on attachments, enforced by `chatflow.attachment.put` and the turn/steer methods (the UI
/// checks them first, to say so before anything is sent).
pub mod limits {
    /// Largest file put into the attachment store (bytes): a pasted or dropped file, or a file
    /// outside the project that is copied there.
    pub const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;
    /// Attachments one message may carry.
    pub const MAX_ATTACHMENTS: usize = 20;
    /// Largest preview (`thumb`, a small `data:image/` URL the UI makes for the transcript).
    pub const MAX_THUMB_BYTES: usize = 32 * 1024;
    /// Pasted text longer than this (characters or lines) becomes a text-file attachment instead
    /// of message text (the composer's defaults; its `composer.pasteAsFile` setting overrides them).
    pub const PASTE_AS_FILE_CHARS: usize = 8000;
    pub const PASTE_AS_FILE_LINES: usize = 200;
}

/// Kinds of [`Attachment`].
pub mod attachment_kinds {
    pub const IMAGE: &str = "image";
    pub const FILE: &str = "file";
    pub const FOLDER: &str = "folder";
}

/// A file, image or folder attached to a user message (`chatflow.turn`, `chatflow.steer`,
/// `agents.steer`), journaled on the `user_message` (or `steer`) entry: `{text, attachments}`.
/// The model gets files and folders as paths it reads with its tools. An image goes to it as a
/// file (path, size, dimensions) unless `model.info` advertises image input
/// ([`super::model::ModelInfo::accepts_images`]); then also as an image content part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Attachment {
    /// image | file | folder ([`attachment_kinds`]).
    pub kind: String,
    /// Absolute path: the stored copy in the attachment store, or the user's own file or folder.
    pub path: String,
    /// The name the user knows it by (the file name, or e.g. `screenshot.png` for a paste).
    pub name: String,
    #[serde(default)]
    pub mime: String,
    /// Size in bytes (0 for a folder).
    #[serde(default)]
    pub bytes: u64,
    /// SHA-256 (hex) of the content (absent for a folder or a project file over the size limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// True when the bytes are in the attachment store; false when `path` is the user's own.
    #[serde(default)]
    pub stored: bool,
    /// Image dimensions in pixels, when the header could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// A small preview the UI made (a `data:image/` URL of at most [`limits::MAX_THUMB_BYTES`]),
    /// shown in the transcript; never sent to the model. The image itself is never in the journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<String>,
    /// Set by the chat flow when it journals the message (ignored on input): a folder's top-level
    /// listing within a small budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listing: Option<String>,
    /// Set by the chat flow when it journals the message (ignored on input): what a vision
    /// capability said the image shows, and which one said it. Absent when none answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub described_by: Option<String>,
    /// Text the user pasted that was too long to stay in the message (the composer made it a file).
    /// The model gets its path, its line count and a short head/tail [`Self::preview`], and reads
    /// or searches the rest with its tools.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pasted_text: bool,
    /// Set by the chat flow when it journals pasted text (ignored on input): its line count and
    /// its first and last lines within a small budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Set by the chat flow when it journals a text attachment (ignored on input): when the text is
    /// a test run's output too long to show whole, its compact report (the counts and one line per
    /// failing test), shown to the model in place of the head/tail preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_report: Option<String>,
    /// Set by the chat flow (ignored on input): for test output shown whole, what a failure in it
    /// means where the runner's words hide it, and tests the runner could not reach; shown after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_notes: Option<String>,
}

/// `chatflow.attachment.put`: exactly one of `path` (a local file or folder the user picked or
/// dropped, or a file the shell wrote to `attachments/incoming/`) and `data` (base64 bytes, the
/// fallback where no shell can write the bytes to a file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AttachmentPutParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Base64 (standard alphabet) of the bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// The name to show; defaults to the path's file name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime: String,
    /// The project the message is for (its root decides whether a path is the user's repository).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// The session, when there is one (its workspace counts as the repository too).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// A preview the UI made (see [`Attachment::thumb`]); returned with the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<String>,
}

/// What `chatflow.attachment.put` stored or referenced: `{sha256, path, mime, size, width?,
/// height?}` (the planned `frame.attachment.put` shape) plus what a message attachment needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AttachmentPutResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub path: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// image | file | folder.
    pub kind: String,
    pub name: String,
    /// True when the bytes are in the attachment store; false when `path` is the user's own.
    #[serde(default)]
    pub stored: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<String>,
}

impl From<AttachmentPutResult> for Attachment {
    fn from(put: AttachmentPutResult) -> Self {
        Attachment {
            kind: put.kind,
            path: put.path,
            name: put.name,
            mime: put.mime,
            bytes: put.size,
            sha256: put.sha256,
            stored: put.stored,
            width: put.width,
            height: put.height,
            thumb: put.thumb,
            ..Attachment::default()
        }
    }
}

/// Journal entry kinds the chat flow writes beyond the frame's.
pub mod kinds {
    /// `{call_id, tool, args, classes, question, state: "pending"|"approved"|"denied", run_id}`: an
    /// ask-mode tool call waiting for the user. The UI answers with `chatflow.approve`.
    pub const APPROVAL: &str = "approval";
    /// `{system, tools, versions, data_version, ...}`: the session's frozen prompt prefix.
    pub const CONTEXT: &str = "context";
    /// One version of the session's spec: `{version, title, text, requirements: [{id, text,
    /// status: open|met|dropped}], decisions: [{decided, because, rejected}], source_seq?, changes,
    /// by: model|user, run_id?}`. The newest entry is the current spec; the model sees it whole,
    /// right after the system prompt, never folded.
    pub const SPEC: &str = "spec";
    /// One version of a plan: `{plan_id, version, doc: {goal, approach, steps, architecture,
    /// risks}, tasks: [{n, title, brief, depends_on: [n], touches, checks, covers, task_id?}],
    /// status: draft|approved|implementing|done, diff, skills, note, root_task?, run_id}`. Status
    /// changes patch the newest version; content changes append a new version.
    pub const PLAN: &str = "plan";
    /// A decision the model proposed from the discussion: `{decided, because, rejected, state:
    /// proposed|accepted|rejected, run_id, knowledge_id?}`, answered with `chatflow.decision.answer`.
    pub const DECISION: &str = "decision";
    /// A knowledge item the model proposed from the user's message (the router's `teaches`):
    /// `{kind: concept|fact|hypothesis|question, name, text, item_state, binds: [code names], state:
    /// proposed|accepted|rejected, run_id, knowledge_id?}`, answered with `chatflow.knowledge.answer`.
    pub const KNOWLEDGE: &str = "knowledge";
}

/// Statuses of a spec requirement.
pub mod requirement_status {
    pub const OPEN: &str = "open";
    pub const MET: &str = "met";
    pub const DROPPED: &str = "dropped";
    pub const ALL: &[&str] = &[OPEN, MET, DROPPED];
}

/// Statuses of a plan.
pub mod plan_status {
    pub const DRAFT: &str = "draft";
    pub const APPROVED: &str = "approved";
    pub const IMPLEMENTING: &str = "implementing";
    pub const DONE: &str = "done";
}

/// One requirement of a spec as the user edits it (`id` absent: a new one).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RequirementEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub text: String,
    /// open | met | dropped ([`requirement_status`]); default open.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
}

/// `chatflow.spec.update`: what is given replaces the current spec's field; `requirements`, when
/// given, is the whole new list (a requirement left out is dropped).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SpecUpdateParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements: Option<Vec<RequirementEdit>>,
    /// The version the edit was made on (0: no spec yet). When the spec has moved on since, the edit
    /// is refused with `conflict` and the current version in `data`, so a newer change is never
    /// overwritten by an edit of an older copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanUpdateParams {
    pub session_id: String,
    pub plan_id: String,
    /// approved | draft
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanRefParams {
    pub session_id: String,
    pub plan_id: String,
    /// ask | auto (the implementing run's mode).
    #[serde(default = "default_mode")]
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionAnswerParams {
    pub session_id: String,
    /// Seq of the `decision` entry.
    pub seq: u64,
    pub accept: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeAnswerParams {
    pub session_id: String,
    /// Seq of the `knowledge` entry.
    pub seq: u64,
    pub accept: bool,
    /// The text as the user edited it before accepting (absent: as proposed).
    #[serde(default)]
    pub text: Option<String>,
}

pub mod topics {
    /// `{ok, version, sources}` when a data set (prompts, texts, tunables) became active, or
    /// `{ok: false, error, version}` when one was rejected (`version` stays the active one).
    pub const DATA: &str = "chatflow.data";
    /// One finished model call: `{ts_ms, session_id, run_id, stage, priority, prompt_tokens,
    /// cached_tokens, completion_tokens, prefill_ms, decode_ms, queue_ms, wall_ms, finish_reason}`.
    /// The agents plugin's concurrency controller reads these live.
    pub const USAGE: &str = "chatflow.usage";
    /// An agent's KV was hibernated while it waited on a long tool call: `{session_id, cache_key}`.
    pub const HIBERNATED: &str = "chatflow.hibernated";
    /// A decision's free-text fields while its JSON streams ([`super::LiveFields`]), so the person
    /// reads what the model is deciding (ROUTE's `work`, DECIDE's `missing`, `brief`, `note`)
    /// before the decision is journaled. At most one event per 250 ms per call, and a last one
    /// with `done: true`.
    pub const LIVE: &str = "chatflow.live";
}

/// One `chatflow.live` event: every top-level string field of the decision written so far (the
/// last one possibly unfinished), in the order written.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LiveFields {
    pub session_id: String,
    pub run_id: String,
    pub stage: String,
    /// `[name, text]` pairs.
    pub fields: Vec<(String, String)>,
    #[serde(default)]
    pub done: bool,
}

/// Answer to a pending `approval` entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApproveParams {
    pub session_id: String,
    /// Seq of the `approval` entry.
    pub seq: u64,
    pub approve: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnParams {
    pub session_id: String,
    pub text: String,
    /// ask | auto
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Made by `chatflow.attach`; journaled on the `user_message` entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// The user marked this message as the session's spec (the composer's Spec toggle).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spec: bool,
    /// Other projects the message references (`@<project>`, `@<project>:<symbol>` in the
    /// composer): each is opened read-only (`memory.project.open` with `reference: true`) and named
    /// to the run in a host note with its root, its qualified URIs and the symbol's location.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Mention>,
}

/// A project (and optionally a symbol in it) referenced by a message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Mention {
    /// The project's id, or its name ([`super::memory::ProjectInfo`]).
    pub project: String,
    /// A symbol in it: a name, a qualified name or a sid (as `memory.symbol` takes it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

fn default_mode() -> String {
    "auto".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnResult {
    pub run_id: String,
    /// True when the text was queued as steering because a run was already active.
    #[serde(default)]
    pub queued_as_steer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SteerParams {
    pub session_id: String,
    pub text: String,
    /// steer | followup
    #[serde(default = "default_steer")]
    pub kind: String,
    /// Made by `chatflow.attach`; journaled on the `steer` entry (and on the `user_message` a
    /// steer or follow-up becomes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Other projects the message references, as on [`TurnParams::mentions`]: opened read-only
    /// and named to the run the steer goes to (or the turn it starts) in a host note.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Mention>,
}

fn default_steer() -> String {
    "steer".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowState {
    pub session_id: String,
    pub running: bool,
    #[serde(default)]
    pub run_id: Option<String>,
    /// Current stage (route, read, decide, act, verify, reply).
    #[serde(default)]
    pub stage: Option<String>,
    pub pending_steers: u32,
    pub pending_followups: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentRunParams {
    pub session_id: String,
    pub task_id: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Run until the task contract is green or the agent gives up; the host verifies.
    #[serde(default)]
    pub options: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentRunResult {
    /// done | failed | cancelled | blocked
    pub outcome: String,
    pub summary: String,
    pub turns: u32,
    pub tool_calls: u32,
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub usage: Value,
}
