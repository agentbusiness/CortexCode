//! `browser@1`: the built-in Chromium browser (Electron `WebContentsView` driven through CDP).
//!
//! Served by the Electron shell, which connects to the frame over WebSocket and says hello as the
//! `browser` category. The profile (cookies, storage, tokens) persists across restarts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod methods {
    /// Open a tab (shown in the right pane, unless [`OpenParams::background`]) and navigate.
    pub const OPEN: &str = "browser.open";
    pub const NAVIGATE: &str = "browser.navigate";
    pub const TABS: &str = "browser.tabs";
    pub const CLOSE: &str = "browser.close";
    /// Readable page state: URL, title, accessibility tree as indented text, with element refs.
    pub const SNAPSHOT: &str = "browser.snapshot";
    /// PNG screenshot written to a file; returns the path.
    pub const SCREENSHOT: &str = "browser.screenshot";
    /// Console messages since the tab opened (or since `since_ms`).
    pub const CONSOLE: &str = "browser.console";
    /// Network requests with status and timing.
    pub const NETWORK: &str = "browser.network";
    /// Evaluate JavaScript in the page; returns the JSON-serialized result.
    pub const EVAL: &str = "browser.eval";
    /// Click an element by ref (from a snapshot) or CSS selector.
    pub const CLICK: &str = "browser.click";
    /// Type text into an element by ref or selector.
    pub const TYPE: &str = "browser.type";
    pub const PRESS: &str = "browser.press";
    /// Performance metrics (CDP Performance.getMetrics).
    pub const METRICS: &str = "browser.metrics";
    /// Raw CDP command on a tab.
    pub const CDP: &str = "browser.cdp";
    /// Show a tab in the browser pane ([`TabParams`] -> [`Tab`]).
    pub const SELECT: &str = "browser.select";
    /// History and reload of a tab ([`TabParams`] -> [`Tab`], after the navigation settles).
    pub const BACK: &str = "browser.back";
    pub const FORWARD: &str = "browser.forward";
    pub const RELOAD: &str = "browser.reload";
}

pub mod topics {
    /// [`super::TabsResult`] whenever a tab opens, closes, navigates, loads or is shown.
    pub const TABS: &str = "browser.tabs";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenParams {
    pub url: String,
    #[serde(default)]
    pub session_id: Option<String>,
    /// A background tab: never shown in the pane nor listed by `browser.tabs`, addressed by its
    /// `tab_id` only (for plugins that read pages, e.g. research's web search). Close it when done.
    #[serde(default)]
    pub background: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tab {
    pub tab_id: String,
    pub url: String,
    pub title: String,
    pub loading: bool,
    /// The session that opened the tab, when one did.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Why the last navigation failed (the tab then shows Chromium's error page).
    #[serde(default)]
    pub load_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabParams {
    pub tab_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NavigateParams {
    pub tab_id: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabsResult {
    pub tabs: Vec<Tab>,
    /// The tab shown in the browser pane.
    #[serde(default)]
    pub active: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotResult {
    pub url: String,
    pub title: String,
    /// Indented accessibility tree; interactive nodes carry `[ref=N]`.
    pub tree: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScreenshotResult {
    pub path: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogParams {
    pub tab_id: String,
    #[serde(default)]
    pub since_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConsoleMessage {
    pub level: String,
    pub text: String,
    pub ts_ms: u64,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConsoleResult {
    pub messages: Vec<ConsoleMessage>,
    /// Older entries omitted by the bounded live history.
    #[serde(default)]
    pub omitted: u64,
    /// Page-controlled fields clipped to the per-field retention limit.
    #[serde(default)]
    pub clipped: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetworkEntry {
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub status: Option<u32>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<f64>,
    #[serde(default)]
    pub failed: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetworkResult {
    pub entries: Vec<NetworkEntry>,
    /// Older entries omitted by the bounded live history.
    #[serde(default)]
    pub omitted: u64,
    /// Page-controlled fields clipped to the per-field retention limit.
    #[serde(default)]
    pub clipped: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalParams {
    pub tab_id: String,
    pub expression: String,
}

/// Result of `browser.eval`: the value (JSON-serialized by the page) and its JavaScript type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalResult {
    #[serde(default)]
    pub value: Value,
    /// `typeof` of the result (`number`, `string`, `object`, `undefined`, ...).
    #[serde(rename = "type", default)]
    pub kind: String,
}

/// Result of `browser.click`, `browser.type` and `browser.press`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionResult {
    pub ok: bool,
}

/// Result of `browser.metrics`: CDP `Performance.getMetrics` as name -> value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricsResult {
    pub metrics: std::collections::BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TargetParams {
    pub tab_id: String,
    /// `ref=N` from a snapshot, or a CSS selector.
    pub target: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PressParams {
    pub tab_id: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CdpParams {
    pub tab_id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}
