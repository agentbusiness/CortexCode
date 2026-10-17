//! Lifecycle methods every plugin answers (handled by `plugin::run_plugin`'s host), and the hello.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod methods {
    pub const HEALTH: &str = "plugin.health";
    pub const PROBE: &str = "plugin.probe";
    pub const ACTIVATE: &str = "plugin.activate";
    pub const DRAIN: &str = "plugin.drain";
    pub const SHUTDOWN: &str = "plugin.shutdown";
    pub const EXPORT_STATE: &str = "plugin.export_state";
    pub const IMPORT_STATE: &str = "plugin.import_state";
}

/// How the frame replaces a running instance of this plugin with a new version.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SwapPolicy {
    /// Start the new instance, probe it, switch traffic, then drain and stop the old one.
    #[default]
    BlueGreen,
    /// Drain and stop the old instance first, then start the new one. For plugins whose two
    /// instances cannot coexist (e.g. a model server holding most of the GPU memory).
    DrainFirst,
}

/// First request a plugin sends (`frame.hello`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    pub plugin_id: String,
    /// Semver of this plugin build.
    pub version: String,
    pub category: String,
    /// `<category>@<major>`.
    pub contract: String,
    /// Methods (`category.verb`) this instance serves.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Event topic prefixes this instance wants delivered.
    #[serde(default)]
    pub subscribes: Vec<String>,
    /// Filled in by `run_plugin` from the environment.
    #[serde(default)]
    pub instance: String,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub swap: SwapPolicy,
}

impl Hello {
    pub fn new(plugin_id: &str, version: &str, category: &str, contract: &str) -> Self {
        Self {
            plugin_id: plugin_id.to_owned(),
            version: version.to_owned(),
            category: category.to_owned(),
            contract: contract.to_owned(),
            provides: Vec::new(),
            subscribes: Vec::new(),
            instance: String::new(),
            pid: 0,
            swap: SwapPolicy::BlueGreen,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HelloAck {
    pub accepted: bool,
    #[serde(default)]
    pub reason: String,
    pub instance: String,
    pub frame_version: String,
    /// The plugin's `[plugin.config]` table from the frame config (`null` when absent). Machine
    /// facts come from `frame.machine`; paths come from the process environment.
    #[serde(default)]
    pub config: Value,
}

/// Result of `plugin.probe`: the plugin's own conformance self-test, run for real.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeReport {
    pub ok: bool,
    pub checks: Vec<ProbeCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeCheck {
    pub name: String,
    pub ok: bool,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub ms: u64,
}

impl ProbeReport {
    pub fn from_checks(checks: Vec<ProbeCheck>) -> Self {
        Self { ok: !checks.is_empty() && checks.iter().all(|check| check.ok), checks }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Health {
    pub status: String,
    pub in_flight: usize,
    pub uptime_ms: u64,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DrainParams {
    #[serde(default = "default_drain_ms")]
    pub timeout_ms: u64,
}

fn default_drain_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DrainResult {
    /// Requests still running when the drain returned (0 = clean).
    pub remaining: usize,
}
