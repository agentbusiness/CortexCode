//! Reports: what an app sends about itself (its errors and its metrics) to a CortexManage that
//! manages its repository. Part of the shared standard (`AGENTS.md`); every app sends, CortexManage
//! receives.
//!
//! - Every process collects (`stats-kit`) and every [`SUMMARY_MS`] (five minutes, a panic at once)
//!   hands its rows to the app's `stats` plugin (`stats.record`), the one that writes Parquet day
//!   files under the stats folder. The tables are exactly [`tables::ERRORS`] (errors and panics,
//!   grouped per window by fingerprint, and the call sites that flood the log; warnings are never
//!   collected), [`tables::CALLS`] (every contract method, summed per five-minute bucket) and
//!   [`tables::MODELS`] (the model calls made, summed per five-minute bucket by model, tag and
//!   computer). No app sends a table of its own.
//! - Beside each day file a `<day>.sent.parquet` lists the event ids that a CortexManage accepted.
//!   A row not in it is undelivered.
//! - After each hand-over the `stats` plugin (in the knowledge app, its mesh) offers the rows just
//!   written, one table of one day at a time, to each CortexManage that shares [`SERVICE`], this
//!   machine first; the day files are read again only for rows a round could not deliver. One that
//!   manages the sender's repository answers `accepted` once the rows are kept; the rows are then
//!   marked sent. One that does not answers `accepted: false`, the next is asked, and it is not
//!   asked again for an hour. With none, the rows wait on disk.
//! - The repository is the one the app declares (`release::DEFAULT_REPO`, `owner/name`); a
//!   CortexManage matches it exactly against the GitHub repository a person set on a managed
//!   repository. Nothing is matched by name or pattern.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The service a CortexManage shares for reports.
pub const SERVICE: &str = "manage.reports";
pub const VERSION: u32 = 1;

/// The window of a summary row, in milliseconds: `calls` and `models` rows each sum one bucket of
/// this length that starts at a multiple of it (UTC), and every process hands its rows over once
/// per window.
pub const SUMMARY_MS: i64 = 300_000;

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

/// The tables `stats-kit` writes and a CortexManage keeps; there are no others.
pub mod tables {
    pub const ERRORS: &str = "errors";
    pub const CALLS: &str = "calls";
    /// The model calls a process made, summed per five-minute bucket: the model, the tag, the
    /// computer that served them, their tokens and times.
    pub const MODELS: &str = "models";
    /// Every table, the only ones a CortexManage accepts.
    pub const ALL: [&str; 3] = [ERRORS, CALLS, MODELS];
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

/// One group of the same failure at the same place within one window (the `errors` table).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ErrorRecord {
    pub schema_version: u32,
    pub event_id: String,
    /// `server` or `interface`.
    pub source: String,
    /// `error`, `panic`, `http_status`, `log_flood`, `collector_dropped`, or an interface kind
    /// (`ui_error`, ...). Warnings are never collected.
    pub kind: String,
    /// The part of the software (a module, a route's first segment, a view).
    pub area: String,
    /// The log target (module path), the route, or the interface component.
    pub target: String,
    /// Where it happened: `file:line`, `METHOD /route`, or the bundle frame.
    pub location: String,
    /// Stable id of the group across windows and days.
    pub fingerprint: String,
    /// `ERROR` or `PANIC`; for a `log_flood`, the level of the flooding lines.
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
    /// A failure to repair: an error or a panic. A log flood is kept as a metric.
    pub fn is_failure(&self) -> bool {
        self.level == "ERROR" || self.level == "PANIC"
    }
}
