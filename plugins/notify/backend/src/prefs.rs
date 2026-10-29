//! The user's notification preferences. They live in the main settings database (`memory.settings.*`,
//! section `notify`): `notify.kinds.<kind>` (boolean), `notify.only_when_away` (boolean),
//! `notify.long_job_ms` and `notify.dedup_window_ms` (numbers). When the settings database cannot be
//! read the built-in defaults below apply, and every notification says so in its detail.

use cc_protocol::contracts::notify::{KindPref, PrefsResult, kinds};
use serde_json::Value;
use std::collections::BTreeMap;

/// Settings section of this plugin.
pub const SECTION: &str = "notify";

/// A kind the user can switch on or off.
pub struct KindInfo {
    pub kind: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub default: bool,
}

/// Every switchable kind, in the order the settings panel lists them.
pub const KINDS: &[KindInfo] = &[
    KindInfo { kind: kinds::DONE, label: "Run finished", description: "A conversation turn or an agent run finished.", default: true },
    KindInfo { kind: kinds::FAILED, label: "Run failed", description: "A run stopped with an error, with the reason.", default: true },
    KindInfo { kind: kinds::APPROVAL, label: "Approval needed", description: "A command (ask mode) waits for your approval.", default: true },
    KindInfo { kind: kinds::BLOCKED, label: "Agent blocked", description: "An agent cannot continue without you.", default: true },
    KindInfo { kind: kinds::VERIFIED, label: "Contract verified", description: "A task's contract checks passed.", default: true },
    KindInfo { kind: kinds::VERIFY_FAILED, label: "Contract failed", description: "A task's contract checks failed, with the failing checks.", default: true },
    KindInfo { kind: kinds::CONFLICT, label: "Merge conflict", description: "Merging an agent's work conflicted and went back to it.", default: true },
    KindInfo { kind: kinds::JOB, label: "Long job ended", description: "A background job that ran a while finished or failed.", default: true },
    KindInfo { kind: kinds::SWAP_FAILED, label: "Plugin swap failed", description: "A hot-swap failed or was rolled back.", default: true },
    KindInfo { kind: kinds::REPORT, label: "Report ready", description: "A report announced its result.", default: true },
    KindInfo { kind: kinds::SPAWNED, label: "Agent started", description: "An agent was started on a task.", default: true },
    KindInfo { kind: kinds::CANCELLED, label: "Run cancelled", description: "A run was cancelled or an agent was stopped.", default: true },
    KindInfo { kind: kinds::INFO, label: "Other notices", description: "Anything else a plugin posts.", default: true },
];

pub const DEFAULT_LONG_JOB_MS: u64 = 30_000;
pub const DEFAULT_DEDUP_WINDOW_MS: u64 = 10 * 60_000;

pub fn kind_info(kind: &str) -> Option<&'static KindInfo> {
    KINDS.iter().find(|info| info.kind == kind)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    pub kinds: BTreeMap<String, bool>,
    pub only_when_away: bool,
    pub long_job_ms: u64,
    pub dedup_window_ms: u64,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            kinds: KINDS.iter().map(|info| (info.kind.to_owned(), info.default)).collect(),
            only_when_away: true,
            long_job_ms: DEFAULT_LONG_JOB_MS,
            dedup_window_ms: DEFAULT_DEDUP_WINDOW_MS,
        }
    }
}

impl Prefs {
    /// Whether notifications of `kind` are wanted. Kinds without a switch (kinds a caller of
    /// `notify.send` invents) are delivered.
    pub fn enabled(&self, kind: &str) -> bool {
        self.kinds.get(kind).copied().unwrap_or(true)
    }

    /// Build preferences from `memory.settings.list {prefix: "notify"}` (nested JSON relative to the
    /// section). Values of the wrong JSON type are ignored and reported in the returned problems.
    pub fn from_settings(settings: &Value) -> (Prefs, Vec<String>) {
        let mut prefs = Prefs::default();
        let mut problems = Vec::new();
        let Some(object) = settings.as_object() else {
            if !settings.is_null() {
                problems.push(format!("`{SECTION}` is not a section"));
            }
            return (prefs, problems);
        };
        if let Some(kinds) = object.get("kinds") {
            match kinds.as_object() {
                Some(map) => {
                    for (kind, value) in map {
                        match value.as_bool() {
                            Some(enabled) => {
                                prefs.kinds.insert(kind.clone(), enabled);
                            }
                            None => problems.push(format!("{SECTION}.kinds.{kind} is not a boolean")),
                        }
                    }
                }
                None => problems.push(format!("{SECTION}.kinds is not a section")),
            }
        }
        if let Some(value) = object.get("only_when_away") {
            match value.as_bool() {
                Some(flag) => prefs.only_when_away = flag,
                None => problems.push(format!("{SECTION}.only_when_away is not a boolean")),
            }
        }
        for (name, slot) in [("long_job_ms", &mut prefs.long_job_ms), ("dedup_window_ms", &mut prefs.dedup_window_ms)] {
            if let Some(value) = object.get(name) {
                match value.as_u64().or_else(|| value.as_f64().filter(|v| *v >= 0.0).map(|v| v as u64)) {
                    Some(number) => *slot = number,
                    None => problems.push(format!("{SECTION}.{name} is not a non-negative number")),
                }
            }
        }
        (prefs, problems)
    }

    pub fn to_result(&self, source: &str, detail: &str) -> PrefsResult {
        PrefsResult {
            kinds: KINDS
                .iter()
                .map(|info| KindPref {
                    kind: info.kind.to_owned(),
                    label: info.label.to_owned(),
                    description: info.description.to_owned(),
                    enabled: self.enabled(info.kind),
                    default: info.default,
                })
                .collect(),
            only_when_away: self.only_when_away,
            long_job_ms: self.long_job_ms,
            dedup_window_ms: self.dedup_window_ms,
            source: source.to_owned(),
            detail: detail.to_owned(),
        }
    }
}

/// Settings path of one preference.
pub fn path(name: &str) -> String {
    format!("{SECTION}.{name}")
}
