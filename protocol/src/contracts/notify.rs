//! `notify@1`: host-OS notifications (macOS Notification Center) for things the user should know
//! without watching the app: a run finished or failed, an approval is needed, an agent is blocked,
//! a contract verified or failed, a merge conflicted, a plugin swap rolled back, a report is ready.
//!
//! The notify plugin derives notifications from events (journal patches, `agents.changed`,
//! `plugin.swapped`, `tools.job`), applies the user's preferences (settings section `notify` in the
//! main settings database, `memory.settings.*`), the dedup window and the focus rule, and delivers
//! each one through the CortexCode notification app when macOS allows it; otherwise it stays in
//! CortexCode's own list. A window (the Electron shell) holds a presence lease with
//! [`methods::DELIVERER`] so clicks are handed to it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod methods {
    /// [`super::Notification`] -> [`super::Posted`]. Post a notification (deduplicated by `key`).
    /// Usually derived from events, also callable directly. Answers once delivery has finished.
    pub const SEND: &str = "notify.send";
    /// [`super::HistoryParams`] -> [`super::HistoryResult`]. Recent notifications (newest first)
    /// with their delivery state.
    pub const HISTORY: &str = "notify.history";
    /// [`super::ActivatedParams`] -> [`super::ActivatedResult`]. The user activated a notification
    /// (clicked it / its action); routed back from the deliverer. Publishes [`super::topics::ACTIVATE`].
    pub const ACTIVATED: &str = "notify.activated";
    /// [`super::DelivererParams`] -> [`super::DelivererResult`]. A UI deliverer (the Electron shell)
    /// registers or renews its lease (renew before `ttl_ms` runs out), or detaches.
    pub const DELIVERER: &str = "notify.deliverer";
    /// `{}` -> [`super::PrefsResult`]. The preferences in effect and where they come from.
    pub const PREFS_GET: &str = "notify.prefs.get";
    /// [`super::PrefsSetParams`] -> [`super::PrefsResult`]. Writes through to `memory.settings.set`.
    pub const PREFS_SET: &str = "notify.prefs.set";
    /// `{}` -> [`super::StatusResult`]. Delivery paths, deliverer, focus and preference source.
    pub const STATUS: &str = "notify.status";
}

pub mod topics {
    /// [`super::PostedEvent`] (`{id, ...Notification, ts_ms, update, deliver_to}`) when a
    /// notification is posted. The deliverer named in `deliver_to` shows it; others may list it.
    pub const POSTED: &str = "notify.posted";
    /// [`super::Activate`] (`{id, action, target, ...}`) when the user activated a notification;
    /// the UI opens the target.
    pub const ACTIVATE: &str = "notify.activate";
    /// [`super::FocusState`]: published by the UI (the shell) whenever the window's focus or the
    /// selected session changes, and again as a heartbeat; stale after `ttl_ms`.
    pub const FOCUS: &str = "notify.focus";
    /// [`super::Posted`] whenever a history item changes (posted, suppressed, delivered, activated).
    pub const CHANGED: &str = "notify.changed";
}

/// Notification kinds the plugin derives (structural mapping of event fields, see the plugin).
pub mod kinds {
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
    pub const APPROVAL: &str = "approval";
    pub const BLOCKED: &str = "blocked";
    pub const VERIFIED: &str = "verified";
    pub const VERIFY_FAILED: &str = "verify_failed";
    pub const CONFLICT: &str = "conflict";
    pub const SPAWNED: &str = "spawned";
    pub const JOB: &str = "job";
    pub const SWAP_FAILED: &str = "swap_failed";
    pub const REPORT: &str = "report";
    pub const INFO: &str = "info";
}

/// Delivery states of a [`Posted`].
pub mod delivery {
    /// Being delivered.
    pub const PENDING: &str = "pending";
    /// Delivered natively by the backend to the OS notification center.
    pub const OS: &str = "os";
    /// Delivered by a UI deliverer (the Electron shell).
    pub const UI: &str = "ui";
    /// Not delivered on purpose (preference off, the user is looking at the session); `detail` says why.
    pub const SUPPRESSED: &str = "suppressed";
    /// Every delivery path failed; `detail` says how.
    pub const FAILED: &str = "failed";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    /// done | failed | cancelled | approval | blocked | verified | verify_failed | conflict | spawned
    /// | job | swap_failed | report | info | test (see [`kinds`])
    pub kind: String,
    pub title: String,
    pub body: String,
    /// Dedup key: a second send with the same key within the dedup window updates instead of adding.
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// What activation opens, e.g. `{"open":"session","session_id":...}` or an approval target.
    #[serde(default)]
    pub target: Value,
    /// Buttons offered by the OS notification (e.g. "Open").
    #[serde(default)]
    pub actions: Vec<String>,
    /// Second line (project and session / agent).
    #[serde(default)]
    pub subtitle: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Posted {
    pub id: String,
    pub notification: Notification,
    pub ts_ms: u64,
    /// pending | os | ui | suppressed | failed (see [`delivery`]); reason in `detail`
    pub delivery: String,
    #[serde(default)]
    pub detail: String,
    /// What it was derived from (`journal:<session>#<seq>`, `agents.changed`, `notify.send`, ...).
    #[serde(default)]
    pub source: String,
    /// The path that delivered it: `helper` (the bundled notification app), `osascript`, `electron`.
    #[serde(default)]
    pub method: String,
    /// Times a repeat within the dedup window updated this item.
    #[serde(default)]
    pub updates: u32,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub activated_ms: Option<u64>,
    #[serde(default)]
    pub activated_action: Option<String>,
}

/// Data of [`topics::POSTED`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostedEvent {
    pub id: String,
    #[serde(flatten)]
    pub notification: Notification,
    pub ts_ms: u64,
    /// True when this replaces an earlier notification with the same id (dedup update).
    #[serde(default)]
    pub update: bool,
    /// The deliverer that should show it (`None`: delivered natively, deliverers only list it).
    #[serde(default)]
    pub deliver_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HistoryParams {
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryResult {
    pub items: Vec<Posted>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivatedParams {
    pub id: String,
    #[serde(default)]
    pub action: Option<String>,
    /// Who reports it: `electron`, `helper`, `history` (the settings panel's Open).
    #[serde(default)]
    pub via: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivatedResult {
    pub routed: bool,
    pub target: Value,
}

/// Data of [`topics::ACTIVATE`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Activate {
    pub id: String,
    #[serde(default)]
    pub action: Option<String>,
    pub target: Value,
    pub kind: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub via: Option<String>,
}

/// Data of [`topics::FOCUS`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FocusState {
    /// Who publishes it (e.g. `electron-shell`).
    #[serde(default)]
    pub source: String,
    /// The app window has keyboard focus.
    pub window_focused: bool,
    /// The app window is on screen (not hidden or minimized).
    #[serde(default = "default_true")]
    pub visible: bool,
    /// The session on screen.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub ts_ms: u64,
    /// The state is stale (treated as "not looking") after this long without a heartbeat.
    #[serde(default = "default_focus_ttl")]
    pub ttl_ms: u64,
}

fn default_true() -> bool {
    true
}

fn default_focus_ttl() -> u64 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DelivererParams {
    pub deliverer_id: String,
    /// `electron`
    #[serde(default)]
    pub kind: String,
    /// The lease lasts this long unless renewed.
    #[serde(default = "default_lease")]
    pub ttl_ms: u64,
    /// End the lease now (the shell is unloading).
    #[serde(default)]
    pub detach: bool,
    /// The deliverer shows action buttons.
    #[serde(default)]
    pub actions: bool,
    #[serde(default)]
    pub detail: String,
}

fn default_lease() -> u64 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DelivererResult {
    /// Whether this deliverer is now the one notifications are handed to.
    pub active: bool,
    pub lease_ms: u64,
}

/// One kind's preference as the panel shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KindPref {
    pub kind: String,
    pub label: String,
    pub description: String,
    pub enabled: bool,
    pub default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrefsResult {
    pub kinds: Vec<KindPref>,
    /// Suppress a notification while the user is looking at its session.
    pub only_when_away: bool,
    /// A background job must have run at least this long to notify when it ends.
    pub long_job_ms: u64,
    /// Repeats with the same key within this window update instead of adding.
    pub dedup_window_ms: u64,
    /// `settings` (the main settings database) or `defaults` (built in; `detail` says why).
    pub source: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PrefsSetParams {
    /// A kind to switch on or off (with `enabled`).
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub only_when_away: Option<bool>,
    #[serde(default)]
    pub long_job_ms: Option<u64>,
    #[serde(default)]
    pub dedup_window_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeStatus {
    /// The method used now: `helper`, `osascript` or `none`.
    pub method: String,
    /// Whether a click on a natively delivered notification comes back to CortexCode.
    pub activation_capable: bool,
    /// Path of the bundled notification app (empty when absent).
    #[serde(default)]
    pub helper_path: String,
    /// The helper's notification authorization: authorized | denied | not_determined | provisional
    /// | absent | error.
    #[serde(default)]
    pub authorization: String,
    #[serde(default)]
    pub osascript: bool,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DelivererInfo {
    pub deliverer_id: String,
    pub kind: String,
    pub age_ms: u64,
    pub ttl_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusResult {
    pub native: NativeStatus,
    #[serde(default)]
    pub deliverer: Option<DelivererInfo>,
    #[serde(default)]
    pub focus: Option<FocusState>,
    /// Milliseconds since the focus state arrived.
    #[serde(default)]
    pub focus_age_ms: Option<u64>,
    pub prefs_source: String,
    #[serde(default)]
    pub prefs_detail: String,
    pub history: usize,
}
