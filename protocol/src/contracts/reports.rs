//! Reports: what an app sends about itself (its errors, warnings and metrics) to a CortexManage that
//! manages its repository. Part of the shared standard (`AGENTS.md`); every app sends, CortexManage
//! receives.
//!
//! - Every process collects (`stats-kit`) and once a minute hands its rows to the app's `stats`
//!   plugin (`stats.record`), the one that writes Parquet day files under the stats folder:
//!   `errors` (warnings, errors and panics, grouped per minute by fingerprint), `calls` (every
//!   contract method, counted per minute) and any table of its own (the knowledge app: `product`, `llm`).
//! - Beside each day file a `<day>.sent.parquet` lists the event ids that a CortexManage accepted.
//!   A row not in it is undelivered.
//! - Once a minute the `stats` plugin (in the knowledge app, its mesh) offers the undelivered rows, one table of one day at a time, to
//!   each CortexManage that shares [`SERVICE`], this machine first. One that manages the sender's
//!   repository answers `accepted`; the rows are then marked sent. One that does not answers
//!   `accepted: false` and the next is asked. With none, the rows wait for the next minute.
//! - The repository is the one the app declares (`release::DEFAULT_REPO`, `owner/name`); a
//!   CortexManage matches it exactly against the GitHub repository a person set on a managed
//!   repository. Nothing is matched by name or pattern.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The service a CortexManage shares for reports.
pub const SERVICE: &str = "manage.reports";
pub const VERSION: u32 = 1;

pub mod methods {
    /// [`super::SendParams`] -> [`super::SendResult`]. Any member may call it (verb `send`).
    pub const SEND: &str = "manage.reports.send";
    /// [`super::RecordParams`] -> `{written}`: a process of this app hands its rows to the app's
    /// `stats` plugin, which writes them and delivers them.
    pub const RECORD: &str = "stats.record";
}

/// The plugin that writes this app's day files and delivers them (`stats@1`).
pub const STATS: &str = "stats";

/// Rows one process collected for one table (`stats.record`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RecordParams {
    pub table: String,
    /// The process (`cc-manage`, `frame`): the folder under the table its rows go to.
    pub part: String,
    pub rows: Vec<Value>,
}

/// The tables `stats-kit` writes; an app may send others of its own.
pub mod tables {
    pub const ERRORS: &str = "errors";
    pub const CALLS: &str = "calls";
}

/// Who sends: the app, as it declares itself, and the computer it runs on.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Sender {
    /// The app's GitHub repository, `owner/name` (`release::DEFAULT_REPO`).
    pub repo: String,
    /// Its mesh software id (`cortexcode`).
    pub software: String,
    /// The app's version.
    pub version: String,
    pub machine: Machine,
}

/// The computer, from the sender's mesh card.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Machine {
    pub node: String,
    pub name: String,
    #[serde(default)]
    pub chip: String,
    #[serde(default)]
    pub os_version: String,
    #[serde(default)]
    pub memory_bytes: u64,
    #[serde(default)]
    pub cpu_cores: u32,
    #[serde(default)]
    pub gpu_cores: u32,
}

/// Rows of one table of one day. Each row is an object with an `event_id`; for `errors` it is an
/// [`ErrorRecord`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SendParams {
    pub sender: Sender,
    pub table: String,
    /// The folders between the table and the day file: the process (`cc-manage`), a kind.
    #[serde(default)]
    pub part: String,
    /// `YYYY-MM-DD` (UTC).
    pub day: String,
    pub rows: Vec<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SendResult {
    /// This CortexManage manages the sender's repository and keeps the rows.
    pub accepted: bool,
    /// Why not, when not accepted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// One group of the same failure at the same place within one minute (the `errors` table).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ErrorRecord {
    pub schema_version: u32,
    pub event_id: String,
    /// `server` or `interface`.
    pub source: String,
    /// `error`, `warning`, `panic`, `http_status`, `log_flood`, `collector_dropped`, or an
    /// interface kind (`ui_error`, ...).
    pub kind: String,
    /// The part of the software (a module, a route's first segment, a view).
    pub area: String,
    /// The log target (module path), the route, or the interface component.
    pub target: String,
    /// Where it happened: `file:line`, `METHOD /route`, or the bundle frame.
    pub location: String,
    /// Stable id of the group across windows and days.
    pub fingerprint: String,
    /// `ERROR`, `WARN`, `PANIC`, ...
    pub level: String,
    /// The first message of the window.
    pub message: String,
    /// The window's distinct messages, the first five.
    #[serde(default)]
    pub samples: Vec<String>,
    /// The event's other fields, as logged.
    #[serde(default)]
    pub fields: Value,
    /// What was going on: spans, thread, run, ...
    #[serde(default)]
    pub scenario: Value,
    #[serde(default)]
    pub stack: Option<String>,
    pub count: u64,
    pub first_at_ms: i64,
    pub last_at_ms: i64,
    /// The version of the part that saw it.
    pub version: String,
    pub node_id: String,
}

impl ErrorRecord {
    /// A failure to repair: an error or a panic. Warnings and the rest are kept as metrics.
    pub fn is_failure(&self) -> bool {
        self.level == "ERROR" || self.level == "PANIC"
    }
}
