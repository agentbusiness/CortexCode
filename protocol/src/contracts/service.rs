//! Shared services: how a plugin offers some of its methods to the other apps on this machine and
//! to the members of the mesh, and how they are called. Part of the standard CortexCode and
//! CortexTerm share (both `AGENTS.md`); the mesh plugin enforces it.
//!
//! - A **service** is the first two words of its methods, `<category>.<service>`
//!   (`terminal.command`, `system.schedule`); the frame routes it by the category as any call.
//! - A **method** ends with a verb from a fixed list, and the verb alone says what the method may
//!   do ([`access_of`]): `list`, `get`, `read` look; `create`, `edit`, `delete` change what is
//!   stored; `run`, `stop` make something happen on the machine. A method whose verb is not on the
//!   list cannot be shared.
//! - A plugin that shares answers `<category>.shares` with a [`ShareList`]: each service with a
//!   one-line description and each method with its description and a JSON schema of its params,
//!   written for people and models alike (the host turns them into model tools).
//! - Where a call may come from: `read` from any member; `edit` and `run` only from apps on the
//!   same machine (a session runs where its project is, so its calls are local there). The person
//!   narrows this per service in Settings > Mesh ([`Allowed`]).
//! - Every shared call carries who asked ([`Caller`], stamped by the mesh, never taken from the
//!   caller's word); a run carries [`RunOptions`]; long work answers a [`Job`]; data comes back in
//!   the [`Size`] the caller asks for.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The verb every sharing plugin serves: `<category>.shares` -> [`ShareList`].
pub const SHARES: &str = "shares";

/// What a method may do, from its verb.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// list, get, read: looks, changes nothing.
    Read,
    /// create, edit, delete: changes what the service keeps.
    Edit,
    /// run, stop: makes something happen on the machine.
    Run,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Edit => "edit",
            Access::Run => "run",
        }
    }
}

/// The verbs a shared method may end with, and what each may do.
pub const VERBS: [(&str, Access); 8] = [
    ("list", Access::Read),
    ("get", Access::Read),
    ("read", Access::Read),
    ("create", Access::Edit),
    ("edit", Access::Edit),
    ("delete", Access::Edit),
    ("run", Access::Run),
    ("stop", Access::Run),
];

/// What a method may do, from its last word; None when it is not a shareable verb.
pub fn access_of(method: &str) -> Option<Access> {
    let verb = method.rsplit('.').next()?;
    VERBS.iter().find(|(name, _)| *name == verb).map(|(_, access)| *access)
}

/// The service of a method: its first two words (`system.schedule.runs.list` -> `system.schedule`).
pub fn service_of(method: &str) -> Option<&str> {
    let mut dots = method.match_indices('.').map(|(at, _)| at);
    let _first = dots.next()?;
    let end = dots.next()?;
    Some(&method[..end])
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ShareList {
    pub shares: Vec<Share>,
}

/// One service a plugin shares.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Share {
    /// `<category>.<service>`.
    pub service: String,
    /// The service's major version; a change that breaks a caller is a new one.
    pub version: u32,
    /// A short name for people (`Terminal commands`).
    pub title: String,
    /// One line: what it is for, for people and models.
    pub description: String,
    pub methods: Vec<SharedMethod>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SharedMethod {
    /// The full method name; it must start with the service and end with a verb of [`VERBS`].
    pub method: String,
    /// One or two sentences: what it does and when to use it.
    pub description: String,
    /// JSON schema of its params (an object). The common fields below ([`Caller`],
    /// [`RunOptions`], [`Size`]) need not be listed.
    #[serde(default)]
    pub params: Value,
}

impl Share {
    /// The method's declaration, when this service shares it.
    pub fn method(&self, method: &str) -> Option<&SharedMethod> {
        self.methods.iter().find(|m| m.method == method)
    }

    /// What is wrong with the declaration, if anything: methods outside the service or with a
    /// verb that cannot be shared are not lent.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.service.split('.').count() != 2 {
            out.push(format!("`{}` is not `<category>.<service>`", self.service));
        }
        for m in &self.methods {
            if service_of(&m.method) != Some(self.service.as_str()) {
                out.push(format!("`{}` is not a method of `{}`", m.method, self.service));
            } else if access_of(&m.method).is_none() {
                out.push(format!("`{}` does not end with a shareable verb", m.method));
            }
        }
        out
    }
}

/// Who asked: added to the params of every shared call as `caller` by the mesh of the machine
/// that serves it (a caller's own `caller` is replaced). Absent for a call made inside one app.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Caller {
    /// The asking app's software (`cortexcode`, `cortexterm`).
    pub app: String,
    /// Its mesh node id, and the computer's name.
    pub node: String,
    #[serde(default)]
    pub name: String,
    /// On this machine.
    #[serde(default)]
    pub local: bool,
    /// The session the call is for, when the caller says.
    #[serde(default)]
    pub session_id: String,
    /// person | agent, as the caller says.
    #[serde(default)]
    pub by: String,
}

/// Options every `run` method takes besides its own.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunOptions {
    /// Record a trace of what it does (CPU, memory, GPU, disk, processes, files, hosts); the
    /// answer then names it ([`Job::trace`]).
    #[serde(default)]
    pub monitor: bool,
    /// Run it in the write sandbox (`sandbox-kit`): writes only in its folder, the temp and
    /// package-cache folders, and the folders in `writable`.
    #[serde(default)]
    pub sandboxed: bool,
    #[serde(default)]
    pub writable: Vec<String>,
    /// The person approved it in the asking app's window (after a [`NeedsApproval`] answer). A
    /// model never sets this: the host removes it from model tool calls.
    #[serde(default)]
    pub approved: bool,
    /// Answer at once with the [`Job`] and let it run (read it back with the service's
    /// `jobs.get`); otherwise the call answers when it ends.
    #[serde(default)]
    pub background: bool,
}

/// How much of what a service keeps comes back: the caller asks for the smallest that will do.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Size {
    /// One line, about 120 characters.
    Line,
    /// A few KB: the numbers, phases, what it touched, errors.
    #[default]
    Summary,
    /// About 120 points over time, for a chart.
    Series,
    /// The raw records of a span, capped by a limit.
    Records,
}

/// The answer of a run, and of reading it back later.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    /// waiting | running | done | failed | stopped | refused
    pub state: String,
    pub started: u64,
    #[serde(default)]
    pub ended: Option<u64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: u64,
    /// The last lines of its output (40 at most); the whole of it is kept by the service.
    #[serde(default)]
    pub tail: Vec<String>,
    /// `trace://<id>` and its one line, when monitored.
    #[serde(default)]
    pub trace: Option<String>,
    #[serde(default)]
    pub trace_line: Option<String>,
    /// Set when it did not run because it needs the person's approval.
    #[serde(default)]
    pub needs_approval: Option<NeedsApproval>,
}

/// The provider's own risk check stopped a run: the asking app's window shows this to the person
/// and, on their yes, calls again with `approved: true`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NeedsApproval {
    /// The risk engine's label (`destructive`, `network`, `privileged`, ...).
    pub risk: String,
    pub reason: String,
}

/// The person's switches for one service: which kinds of call it takes from apps on this machine
/// and from other machines. `edit` and `run` from other machines are never taken, whatever is set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Allowed {
    pub local: Vec<Access>,
    pub network: Vec<Access>,
}

impl Default for Allowed {
    fn default() -> Self {
        Allowed { local: vec![Access::Read, Access::Edit, Access::Run], network: vec![Access::Read] }
    }
}

impl Allowed {
    pub fn takes(&self, access: Access, local: bool) -> bool {
        if local { self.local.contains(&access) } else { access == Access::Read && self.network.contains(&access) }
    }
}

/// A service as one model tool: its name is the service's second word (`schedule`), `action`
/// picks the method (`list`, `runs_read`), and the parameters are every method's, merged. One
/// tool per service keeps the tool list short for a small model; the host adds only the services a
/// session may use, and never `approved` or `caller` (the host sets those).
pub fn tool_of(share: &Share) -> (String, String, Value) {
    let name = share.service.split('.').nth(1).unwrap_or(&share.service).to_owned();
    let mut properties = serde_json::Map::new();
    let mut actions = Vec::new();
    let mut lines = Vec::new();
    for m in &share.methods {
        let action = action_of(&share.service, &m.method);
        lines.push(format!("{action}: {}", m.description));
        actions.push(Value::String(action));
        if let Some(props) = m.params.get("properties").and_then(Value::as_object) {
            for (key, schema) in props {
                if key != "approved" && key != "caller" {
                    properties.entry(key.clone()).or_insert_with(|| schema.clone());
                }
            }
        }
    }
    properties.insert("action".into(), serde_json::json!({"type": "string", "enum": actions}));
    let description = format!("{} Actions:\n- {}", share.description, lines.join("\n- "));
    (name, description, serde_json::json!({"type": "object", "properties": properties, "required": ["action"]}))
}

/// `system.schedule.runs.list` in `system.schedule` -> `runs_list`.
pub fn action_of(service: &str, method: &str) -> String {
    method.strip_prefix(service).unwrap_or(method).trim_start_matches('.').replace('.', "_")
}

/// The method a tool's `action` names, when the service has it.
pub fn method_of<'a>(share: &'a Share, action: &str) -> Option<&'a str> {
    share.methods.iter().map(|m| m.method.as_str()).find(|method| action_of(&share.service, method) == action)
}
