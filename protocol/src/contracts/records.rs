//! The memory plugin's methods the frame keeps its records through (part of `memory@1`): the app's
//! projects and sessions, settings, and journal streams of frames before journals moved into files.
//! They belong to the frame's interface: the frame calls them, the memory plugin serves them, and
//! [`super::memory`] re-exports them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod methods {
    /// Settings in the main project, as a node tree (Settings -> SettingSection -> Setting); an object
    /// value is a section with one setting per member.
    /// Paths are dotted: sections, then the key (`model.variant`, `plugins.memory.depth`).
    pub const SETTINGS_GET: &str = "memory.settings.get";
    /// Create or update one setting; sections are created on demand. A value's JSON type is fixed
    /// once set (changing it requires `memory.settings.delete` first).
    pub const SETTINGS_SET: &str = "memory.settings.set";
    /// The app's projects (the frame's records), from the main store: `{projects: [Project]}`.
    pub const APP_PROJECTS: &str = "memory.app.projects";
    /// Create or replace one project record: `{project: Project}`.
    pub const APP_PROJECT_PUT: &str = "memory.app.project.put";
    /// Retire one project record and delete its settings: `{id}`. Its sessions are deleted with
    /// [`APP_SESSIONS_DELETE`] first. The record stays, retired, until the project is opened again.
    pub const APP_PROJECT_DELETE: &str = "memory.app.project.delete";
    /// The projects deleted (retired), for plugins that start after a deletion:
    /// `{}` -> `{projects: [ProjectDeleted]}`.
    pub const APP_DELETED: &str = "memory.app.deleted";
    /// The app's sessions (chats and agent runs), each linked to its project: `{sessions: [Session]}`.
    pub const APP_SESSIONS: &str = "memory.app.sessions";
    /// Create or replace one session record: `{session: Session}`.
    pub const APP_SESSION_PUT: &str = "memory.app.session.put";
    /// Delete session records and their journals: `{ids: [id]}`.
    pub const APP_SESSIONS_DELETE: &str = "memory.app.sessions.delete";
    /// A session's journal patches from an offset: `{session_id, from, max}` -> `{patches, next,
    /// end}` (`end`: nothing after `next`). A session without a journal reads as empty. The frame
    /// reads a stream once, to move it into the session's journal file.
    pub const JOURNAL_READ: &str = "memory.journal.read";
    /// Drop a session's journal stream (after its file is written): `{session_id}` -> `{}`. A
    /// session without a stream is fine.
    pub const JOURNAL_DROP: &str = "memory.journal.drop";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SettingsGetParams {
    pub path: String,
    /// `global` (absent or empty) or a project id: whose settings.
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SettingsSetParams {
    pub path: String,
    pub value: Value,
    /// `global` (absent or empty) or a project id: whose settings.
    #[serde(default)]
    pub scope: String,
    /// Who changed it (`user`, a plugin id, an agent name).
    #[serde(default)]
    pub by: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// [`methods::APP_PROJECT_PUT`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppProjectPutParams {
    pub project: crate::contracts::frame::Project,
}

/// [`methods::APP_PROJECTS`] result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AppProjectsResult {
    pub projects: Vec<crate::contracts::frame::Project>,
}

/// [`methods::APP_DELETED`] result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AppDeletedResult {
    pub projects: Vec<crate::contracts::frame::ProjectDeleted>,
}

/// [`methods::APP_PROJECT_DELETE`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppProjectDeleteParams {
    pub id: String,
}

/// [`methods::APP_SESSION_PUT`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppSessionPutParams {
    pub session: crate::contracts::frame::Session,
}

/// [`methods::APP_SESSIONS`] result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AppSessionsResult {
    pub sessions: Vec<crate::contracts::frame::Session>,
}

/// [`methods::APP_SESSIONS_DELETE`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppSessionsDeleteParams {
    pub ids: Vec<String>,
}

/// [`methods::JOURNAL_READ`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalReadParams {
    pub session_id: String,
    #[serde(default)]
    pub from: u64,
    /// At most this many patches (absent: the plugin's page size).
    #[serde(default)]
    pub max: Option<usize>,
}

/// [`methods::JOURNAL_READ`] result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalReadResult {
    pub patches: Vec<crate::contracts::frame::JournalPatch>,
    pub next: u64,
    pub end: bool,
}

/// [`methods::JOURNAL_DROP`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalDropParams {
    pub session_id: String,
}
