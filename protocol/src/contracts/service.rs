//! Shared services: how a plugin offers some of its methods to the other apps on this machine and
//! to the members of the mesh, and how they are called. Part of the shared standard (`AGENTS.md`);
//! the mesh enforces it.
//!
//! - **Every service is optional.** An app shares what it has and nothing more; no app has to
//!   implement any service, and a new one needs no change in the others. A caller that finds no
//!   provider does the work itself or goes without (`service-kit`'s `client::provider` answers
//!   None).
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
//!   same machine (a session runs where its project is, so its calls are local there). There is
//!   nothing to set: what a plugin declares is always shared under these rules.
//! - Who it is for: the provider declares it ([`Audience`]), so no app names another app's
//!   service to keep it from its model. `models` (the default) may be a model tool; `apps` is for
//!   apps' own code; `peers` is for the apps that share the same service themselves, and the mesh
//!   lists it to and takes its calls from those only.
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
    /// Who it is for; absent in a declaration: `models`.
    #[serde(default, skip_serializing_if = "Audience::is_models")]
    pub audience: Audience,
}

/// Who a service is for, as its provider declares it.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Audience {
    /// Any app, and a host may offer it to its model as a tool.
    #[default]
    Models,
    /// Any app's own code (a check the host makes, work it hands on); never a model tool.
    Apps,
    /// Only apps that share this same service themselves (the provider's own kind on other
    /// computers): the mesh lists it to and takes its calls from those only; never a model tool.
    Peers,
}

impl Audience {
    pub fn is_models(&self) -> bool {
        *self == Audience::Models
    }
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

    /// Whether a host may offer it to its model as a tool.
    pub fn for_models(&self) -> bool {
        self.audience.is_models()
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
    /// What kind of work it is, as the caller decides: `check` (tests, linters, type checks),
    /// `build`, `server` (stays up), `shell` (anything else). Lists group by it; empty is `shell`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub purpose: String,
    /// For a background run: tell the caller ([`topics::COMMAND_MATCHED`]) when its output
    /// matches or falls silent. Refused without `background`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<Watch>,
}

/// When a background run's caller wants to hear about it before it ends.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Watch {
    /// A regular expression over its output lines (at most one event per 5 s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// No output for this many seconds (once per silent stretch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silence_s: Option<u64>,
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
    /// waiting | running | done | failed | stopped | refused | lost (the provider stopped while it
    /// ran: its result is unknown, its output up to then is kept)
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
    /// Two to four lines a model reads as they are, written by the provider: the state, exit code
    /// and time; the peak CPU and memory when traced; the first error; how to read more (and, for
    /// an old job, what is no longer kept).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// When `jobs.get` was asked a `size` or an `at` and the job was traced: the trace in that
    /// size, or what was happening at that moment ([`Moment`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// A traced process tree at one moment (`system.trace.read {id, at}`, `jobs.get {id, at}`),
/// rebuilt from the trace's records.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Moment {
    /// Unix milliseconds.
    pub at: u64,
    #[serde(default)]
    pub processes: Vec<MomentProcess>,
    /// Bytes, the whole tree.
    #[serde(default)]
    pub memory_total: u64,
    /// Files open then (50 at most).
    #[serde(default)]
    pub open: Vec<MomentFile>,
    /// Connections and listening ports then: `host:port` or `listen :port`.
    #[serde(default)]
    pub connections: Vec<String>,
    /// The last output lines before that moment (20 at most).
    #[serde(default)]
    pub last_output: Vec<String>,
    /// A few lines a model reads as they are.
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MomentProcess {
    pub pid: u32,
    pub name: String,
    /// Percent of one core.
    #[serde(default)]
    pub cpu: f64,
    /// Bytes.
    #[serde(default)]
    pub memory: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MomentFile {
    pub pid: u32,
    pub path: String,
    /// read | write | rw
    #[serde(default)]
    pub mode: String,
}

/// Events of shared services a member hears through `mesh.events`: the serving mesh sends each
/// only to the app named by `node` or `owner.node` (and only on the same machine).
pub mod topics {
    /// [`super::CommandEnded`]: a `terminal.command` run ended (also a `lost` one, when its caller
    /// is back).
    pub const COMMAND_ENDED: &str = "terminal.command.ended";
    /// [`super::CommandMatched`]: a background run's output matched its watch or fell silent.
    pub const COMMAND_MATCHED: &str = "terminal.command.matched";
    /// [`super::ScheduleDone`]: a scheduled job ran.
    pub const SCHEDULE_DONE: &str = "system.schedule.done";
    /// [`super::ScheduleChanged`]: a scheduled job was created, changed, paused or removed.
    pub const SCHEDULE_CHANGED: &str = "system.schedule.changed";
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CommandEnded {
    pub id: String,
    /// The asking app's mesh node, from its caller; the mesh sends the event to it only.
    #[serde(default)]
    pub node: String,
    /// The caller's session (from its `caller`).
    #[serde(default)]
    pub session_id: String,
    pub job: Job,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CommandMatched {
    pub id: String,
    /// The asking app's mesh node, from its caller; the mesh sends the event to it only.
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub session_id: String,
    /// pattern | silence
    pub why: String,
    /// The line that matched; the last line before the silence.
    #[serde(default)]
    pub line: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ScheduleDone {
    pub schedule_id: String,
    pub name: String,
    /// The app that created it (its `caller` then); only that app may change it.
    #[serde(default)]
    pub owner: Option<Caller>,
    /// The run: its id is its trace's, read again with `system.schedule.runs.read`.
    pub run: Job,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ScheduleChanged {
    pub schedule_id: String,
    pub name: String,
    #[serde(default)]
    pub owner: Option<Caller>,
    /// created | edited | paused | resumed | deleted
    pub change: String,
}

/// The provider's own risk check stopped a run: the asking app's window shows this to the person
/// and, on their yes, calls again with `approved: true`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NeedsApproval {
    /// The risk engine's label (`destructive`, `network`, `privileged`, ...).
    pub risk: String,
    pub reason: String,
}

/// A service as one model tool: its name is the service's second word (`schedule`), `action`
/// picks the method (`list`, `runs_read`), and the parameters are every method's, merged. One
/// tool per service keeps the tool list short for a small model; the host adds only the services a
/// session may use ([`Share::for_models`]), and never `approved` or `caller` (the host sets those).
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
