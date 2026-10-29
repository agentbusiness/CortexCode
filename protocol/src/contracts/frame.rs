//! Methods the frame itself serves (to plugins over the Unix socket and to UI clients over the
//! WebSocket), plus the project, session and journal model the frame owns.
//!
//! The journal is the single source of truth for session state: plugins keep no private session
//! state, so any plugin can be hot-swapped and its successor re-materializes from the journal.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Methods the frame serves. Each line names the params and result types.
pub mod methods {
    /// Trusted tools backend requests the restricted API credential for a command session.
    /// [`super::CommandAccessParams`] -> [`super::CommandAccessResult`].
    pub const COMMAND_ACCESS: &str = "frame.command_access";
    /// Atomically activate an already-built set, restoring previous versions on failure.
    /// [`super::InstallManyParams`] -> [`super::InstallManyResult`].
    pub const PLUGINS_INSTALL_MANY: &str = "frame.plugins.install_many";
    /// Wait for a previously submitted installation, including across a caller hot swap.
    /// [`super::InstallWaitParams`] -> [`super::InstallManyResult`].
    pub const PLUGINS_INSTALL_WAIT: &str = "frame.plugins.install_wait";
    /// [`super::super::lifecycle::Hello`] -> [`super::super::lifecycle::HelloAck`]. Plugins send it
    /// first; a WebSocket client sends it to serve a category (the Electron shell serves `browser`).
    pub const HELLO: &str = "frame.hello";

    /// [`super::ProjectOpenParams`] -> [`super::Project`]. The id is stable per root path.
    pub const PROJECT_OPEN: &str = "frame.project.open";
    /// `{}` -> [`super::ProjectListResult`].
    pub const PROJECT_LIST: &str = "frame.project.list";
    /// [`super::ProjectGetParams`] -> [`super::Project`].
    pub const PROJECT_GET: &str = "frame.project.get";
    /// Forget a project and delete all of its sessions (as `frame.session.delete`); publishes
    /// `project.deleted` so every plugin drops its own data about the project. The project's folder
    /// on disk is never touched. [`super::ProjectGetParams`] -> [`super::DeleteResult`].
    pub const PROJECT_DELETE: &str = "frame.project.delete";

    /// [`super::SessionCreateParams`] -> [`super::Session`].
    pub const SESSION_CREATE: &str = "frame.session.create";
    /// [`super::SessionListParams`] -> [`super::SessionListResult`] (oldest first).
    pub const SESSION_LIST: &str = "frame.session.list";
    /// [`super::SessionGetParams`] -> [`super::Session`].
    pub const SESSION_GET: &str = "frame.session.get";
    /// [`super::SessionUpdateParams`] -> [`super::Session`].
    pub const SESSION_UPDATE: &str = "frame.session.update";
    /// Delete a session, the sessions started from it (`parent`), and their journals; publishes
    /// `session.deleted` for each so every plugin drops its own data about it (a running turn is
    /// stopped by its plugin). [`super::SessionGetParams`] -> [`super::DeleteResult`].
    pub const SESSION_DELETE: &str = "frame.session.delete";

    /// Append entries / patches to a session journal (plugins).
    /// [`super::JournalAppendParams`] -> [`super::JournalAppendResult`]; answered after the patch is durable.
    pub const JOURNAL_APPEND: &str = "frame.journal.append";
    /// [`super::JournalPatchParams`] -> [`super::JournalAppendResult`] (the patched entry's seq).
    pub const JOURNAL_PATCH: &str = "frame.journal.patch";
    /// Read materialized entries of a session, optionally from a sequence number.
    /// [`super::JournalReadParams`] -> [`super::JournalReadResult`].
    pub const JOURNAL_READ: &str = "frame.journal.read";

    /// `{}` -> [`super::PluginListResult`].
    pub const PLUGINS_LIST: &str = "frame.plugins.list";
    /// Restart a plugin's backend on the package version it runs (blue/green or drain-first).
    /// [`super::SwapParams`] -> [`super::SwapResult`] (returned when the swap has finished).
    pub const PLUGINS_SWAP: &str = "frame.plugins.swap";
    /// Roll a plugin (backend and UI together) back to the package version the last install
    /// replaced. [`super::RollbackParams`] -> [`super::PackageSwapResult`].
    pub const PLUGINS_ROLLBACK: &str = "frame.plugins.rollback";
    /// `{}` -> [`super::UiPluginListResult`].
    pub const UI_PLUGINS_LIST: &str = "frame.ui_plugins.list";

    /// Disable a plugin (the vertical unit: its backend and every UI package and shell module of
    /// its package). The backend stops, its UI packages are no longer served (`ui_plugin.changed`
    /// with `removed`), calls to its category answer a retryable `unavailable` error, and the state
    /// persists across frame restarts. [`super::PluginToggleParams`] -> [`super::PluginToggleResult`].
    pub const PLUGINS_DISABLE: &str = "frame.plugins.disable";
    /// Enable a disabled plugin again: its backend starts and its UI packages are served.
    /// [`super::PluginToggleParams`] -> [`super::PluginToggleResult`].
    pub const PLUGINS_ENABLE: &str = "frame.plugins.enable";
    /// Install a `.ccplugin` package: unpack it to `<CORTEX_HOME>/plugins-store/<id>/<version>/`,
    /// then hot-swap the plugin's backend and UI packages together (a plugin new to this frame is
    /// added and started); the version it replaces is kept so `frame.plugins.rollback {plugin}`
    /// restores both. A `.ccplugin` dropped into `<CORTEX_HOME>/plugins-store/incoming/` is
    /// installed the same way. [`super::InstallParams`] -> [`super::PackageSwapResult`].
    pub const PLUGINS_INSTALL: &str = "frame.plugins.install";
    /// The plugins installed for one category and which one serves it (a category such as the
    /// model can have several: one serves, the others wait as alternatives).
    /// [`super::ProvidersParams`] -> [`super::ProvidersResult`].
    pub const PLUGINS_PROVIDERS: &str = "frame.plugins.providers";
    /// The plugin packages the app itself carries (`<packages>/<id>@<version>.ccplugin`, installed
    /// at start): what a reset falls back to when no official release is reachable.
    /// `{}` -> [`super::BundledResult`].
    pub const PLUGINS_BUNDLED: &str = "frame.plugins.bundled";
    /// Serve a category with another of its installed plugins: the serving plugin is drained and
    /// stopped (its idle state handed over), the chosen one started in the same slot, and the UI
    /// packages follow. On failure the previous plugin is restored. The choice persists.
    /// [`super::SwitchParams`] -> [`super::SwitchResult`].
    pub const PLUGINS_SWITCH: &str = "frame.plugins.switch";

    /// The workbench's durable slot state (what the UI must not forget across hot swaps and app
    /// restarts), kept by the frame in `<CORTEX_HOME>/state/ui_state.json`.
    /// `{}` -> [`super::UiStateDocument`].
    pub const UI_STATE_GET: &str = "frame.ui_state.get";
    /// Replace the slot state document. [`super::UiStatePutParams`] -> [`super::UiStatePutResult`].
    pub const UI_STATE_PUT: &str = "frame.ui_state.put";

    /// Subscribe a UI client to event topic prefixes (plugins declare theirs in hello).
    /// [`super::SubscribeParams`] -> [`super::SubscribeResult`]. Prefixes accumulate per connection.
    pub const SUBSCRIBE: &str = "frame.subscribe";

    /// Machine facts measured at startup (chip, memory, GPU working set, cores).
    /// `{}` -> [`super::Machine`].
    pub const MACHINE: &str = "frame.machine";
    /// Hold or release a keep-awake assertion owned by the caller.
    /// [`super::KeepAwakeParams`] -> [`super::KeepAwakeResult`]. A holder is released automatically
    /// when the connection that holds it closes. The frame itself holds keep-awake (holder
    /// `frame`, reserved) for as long as it runs, so a caller's hold only adds its reason.
    pub const KEEP_AWAKE: &str = "frame.keep_awake";
    /// Start the Electron shell again after the user closed it (no-op while it runs).
    /// `{}` -> [`super::UiOpenResult`].
    pub const UI_OPEN: &str = "frame.ui.open";
    /// [`super::RouteSetParams`] -> `{}`: route one session's calls of a category to a client
    /// that serves it with scope `sessions` (a member of the mesh lending its model to one agent).
    pub const ROUTE_SET: &str = "frame.route.set";
    /// [`super::RouteClearParams`] -> `{}`.
    pub const ROUTE_CLEAR: &str = "frame.route.clear";
    /// Unpack a downloaded frame release beside the running frame (`<home>/frame/<version>/`,
    /// Electron in `<home>/electron/<version>/`); it runs from the next start.
    /// [`super::UpdateStageParams`] -> [`super::UpdateStatus`].
    pub const UPDATE_STAGE: &str = "frame.update.stage";
    /// The running frame version and the newest staged one. `{}` -> [`super::UpdateStatus`].
    pub const UPDATE_STATUS: &str = "frame.update.status";
    /// Stop every plugin and the window as on quit, then start again on the newest staged frame.
    /// `{}` -> [`super::UpdateStatus`] (answered before the shutdown begins).
    pub const UPDATE_RESTART: &str = "frame.update.restart";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateStageParams {
    /// The frame release version (`0.2.0`).
    pub version: String,
    /// Absolute path of the downloaded frame archive (`.tar.gz`).
    pub frame_archive: String,
    /// The Electron version the frame names.
    pub electron_version: String,
    /// Absolute path of the downloaded Electron archive; empty when that Electron is staged already.
    #[serde(default)]
    pub electron_archive: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UpdateStatus {
    /// The version of the running frame.
    pub running: String,
    /// The newest staged frame newer than the running one (it starts at the next start).
    #[serde(default)]
    pub staged: Option<String>,
    /// Electron versions staged in `<home>/electron/`.
    #[serde(default)]
    pub electron: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandAccessParams {
    pub session_id: String,
    pub workspace: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandAccessResult {
    pub url: String,
    pub token: String,
    pub socket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstallManyParams {
    pub id: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstallWaitParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstallManyResult {
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub swaps: Vec<PackageSwapResult>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub recovery_errors: Vec<String>,
}

/// Event topics the frame publishes.
pub mod topics {
    /// `journal.<session_id>`: one [`super::JournalPatch`] per event.
    pub const JOURNAL_PREFIX: &str = "journal.";
    pub const SESSION_CHANGED: &str = "session.changed";
    pub const PROJECT_CHANGED: &str = "project.changed";
    /// A session was deleted. Data: [`super::SessionDeleted`].
    pub const SESSION_DELETED: &str = "session.deleted";
    /// A project was deleted (its sessions each had `session.deleted` first). Data: [`super::ProjectDeleted`].
    pub const PROJECT_DELETED: &str = "project.deleted";
    /// A plugin instance changed state (starting, probing, active, draining, stopped, crashed).
    pub const PLUGIN_STATE: &str = "plugin.state";
    /// A hot-swap finished (ok or rolled back).
    /// Data: [`super::SwapResult`]. Also published when the watch window rolls a swap back.
    pub const PLUGIN_SWAPPED: &str = "plugin.swapped";
    /// Data: the new [`super::UiPlugin`] (new hash) when a UI plugin is added or rebuilt, or
    /// `{"id": "<id>", "removed": true}` when it disappeared.
    pub const UI_PLUGIN_CHANGED: &str = "ui_plugin.changed";
    /// The window (the `window@1` package) changed: open workbenches reload. Data: `{"build": "<hash>"}`.
    pub const UI_RENDERER_CHANGED: &str = "ui.renderer.changed";
    /// A plugin (vertical unit) was enabled, disabled, installed or rolled back.
    /// Data: the new [`super::PluginUnit`].
    pub const PLUGIN_UNIT: &str = "plugin.unit";
}

/// Ways of working (chat flows) and where their calls go. A flow is a plugin of the `chatflow`
/// family in the category `chatflow-<flow>`; its name is also the tag of the models it asks for
/// (`model::tags`). A project lists the flows that work in it; a session belongs to one, for life.
pub mod flows {
    /// CortexCode's own flow, and every project's and session's from before flows were named.
    pub const CODING: &str = "coding";
    const FAMILY: &str = "chatflow-";

    pub fn default_flow() -> String {
        CODING.to_owned()
    }

    pub fn default_flows() -> Vec<String> {
        vec![default_flow()]
    }

    /// `coding` -> `chatflow-coding`.
    pub fn category(flow: &str) -> String {
        format!("{FAMILY}{flow}")
    }

    /// `coding`, `turn` -> `chatflow-coding.turn`.
    pub fn method(flow: &str, verb: &str) -> String {
        format!("{FAMILY}{flow}.{verb}")
    }

    /// `chatflow-coding` -> `coding`; None for a category of no flow.
    pub fn flow_of(category: &str) -> Option<&str> {
        category.strip_prefix(FAMILY).filter(|flow| !flow.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: String,
    /// Absolute path of the repository root.
    pub root: String,
    pub name: String,
    pub created_ms: u64,
    /// The flows that work in this project ([`flows`]): the flows installed when it was opened.
    #[serde(default = "flows::default_flows")]
    pub flows: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectOpenParams {
    pub root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectGetParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProjectListResult {
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// A conversation with the user (foreground).
    #[default]
    Chat,
    /// A named background agent working a task.
    Agent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Idle,
    Running,
    Waiting,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub kind: SessionKind,
    pub title: String,
    /// Agent name for agent sessions.
    #[serde(default)]
    pub agent: Option<String>,
    /// The session that spawned this one.
    #[serde(default)]
    pub parent: Option<String>,
    /// Working directory the session edits (the project root, or an agent's copy-on-write clone).
    pub workspace: String,
    #[serde(default)]
    pub task_id: Option<String>,
    pub status: SessionStatus,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// The model the session runs on (`model.info` `model_id`), recorded by its first run. A
    /// session is never continued on another model: after an engine or variant switch it stays
    /// readable, and new work starts in a new session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The flow the session belongs to, for life ([`flows`]): its calls go to that flow.
    #[serde(default = "flows::default_flow")]
    pub flow: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionCreateParams {
    pub project_id: String,
    #[serde(default)]
    pub kind: SessionKind,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    /// Defaults to the project root.
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    /// The session's flow: one of the project's. Absent: the parent's, else the project's first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionListParams {
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionGetParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionListResult {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionUpdateParams {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<SessionStatus>,
    #[serde(default)]
    pub task_id: Option<String>,
    /// Record the model the session runs on ([`Session::model`]).
    #[serde(default)]
    pub model: Option<String>,
}

/// What `frame.session.delete` or `frame.project.delete` deleted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DeleteResult {
    /// The sessions deleted, children first.
    pub sessions: Vec<String>,
}

/// `session.deleted`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionDeleted {
    pub session_id: String,
    pub project_id: String,
    /// The folder its commands worked in (an agent's own workspace, or the project root).
    pub workspace: String,
}

/// `project.deleted`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectDeleted {
    pub project_id: String,
    pub root: String,
}

/// Journal entry kinds. `data` schemas are documented per kind.
pub mod kinds {
    /// `{text}`
    pub const USER_MESSAGE: &str = "user_message";
    /// `{text, streaming}` - streamed with `append` patches on field `text`.
    pub const ASSISTANT_MESSAGE: &str = "assistant_message";
    /// `{text, streaming}`
    pub const REASONING: &str = "reasoning";
    /// `{call_id, tool, intent, args}` (args: the model's argument object)
    pub const TOOL_CALL: &str = "tool_call";
    /// `{call_id, ok, output, display?, duration_ms, changed_files?}`
    pub const TOOL_RESULT: &str = "tool_result";
    /// `{text, kind: "steer"|"followup", target, consumed}`
    pub const STEER: &str = "steer";
    /// `{route, detail}` - the router's decision for a foreground turn.
    pub const ROUTE: &str = "route";
    /// `{state, detail}` - run lifecycle: running, done, failed, cancelled.
    pub const STATUS: &str = "status";
    /// `{prompt_tokens, cached_tokens, completion_tokens, prefill_ms, decode_ms, ...}`
    pub const USAGE: &str = "usage";
    /// `{message, code?}`
    pub const ERROR: &str = "error";
    /// `{task_id, title}`
    pub const TASK_LINK: &str = "task_link";
    /// `{agent, task_id, session_id}` - an agent was spawned from this session.
    pub const AGENT_SPAWNED: &str = "agent_spawned";
    /// `{task_id, report}` - contract verification outcome.
    pub const VERIFICATION: &str = "verification";
    /// `{text}` - host note shown in the transcript.
    pub const NOTE: &str = "note";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalEntry {
    /// Sequence number within the session, starting at 1.
    pub seq: u64,
    pub ts_ms: u64,
    pub session_id: String,
    pub kind: String,
    pub data: Value,
}

/// A change to a session journal; journals are append-only sequences of these.
///
/// `rev` is the session journal's revision after this patch: 1 for the first patch of a session,
/// +1 for every patch (create, append or set), never reused. A reader that loaded a snapshot at
/// revision R ([`JournalReadResult::rev`]) applies exactly the patches with `rev > R`, and a gap in
/// the revisions it receives means it missed one. 0 only in journal lines written before revisions
/// existed (the frame renumbers them when it loads the journal).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum JournalPatch {
    /// A new entry.
    Create {
        entry: JournalEntry,
        #[serde(default)]
        rev: u64,
    },
    /// Append text to a string field of an existing entry (streaming).
    Append {
        session_id: String,
        seq: u64,
        field: String,
        text: String,
        #[serde(default)]
        rev: u64,
    },
    /// Set a field of an existing entry.
    Set {
        session_id: String,
        seq: u64,
        field: String,
        value: Value,
        #[serde(default)]
        rev: u64,
    },
}

impl JournalPatch {
    /// The session journal's revision after this patch.
    pub fn rev(&self) -> u64 {
        match self {
            JournalPatch::Create { rev, .. } | JournalPatch::Append { rev, .. } | JournalPatch::Set { rev, .. } => *rev,
        }
    }

    pub fn set_rev(&mut self, value: u64) {
        match self {
            JournalPatch::Create { rev, .. } | JournalPatch::Append { rev, .. } | JournalPatch::Set { rev, .. } => *rev = value,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalAppendParams {
    pub session_id: String,
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalAppendResult {
    pub seq: u64,
}

/// One `append` or `set` on an existing entry. `value` for `set`, `text` for `append`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalPatchParams {
    pub session_id: String,
    pub seq: u64,
    pub field: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalReadParams {
    pub session_id: String,
    /// Return entries with `seq >= from` (default 1).
    #[serde(default)]
    pub from: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalReadResult {
    pub entries: Vec<JournalEntry>,
    /// The journal's revision these entries reflect (see [`JournalPatch`]): apply only patches
    /// with a higher `rev` on top of them.
    #[serde(default)]
    pub rev: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginInfo {
    pub plugin_id: String,
    pub version: String,
    pub category: String,
    pub contract: String,
    pub instance: String,
    /// starting | probing | active | draining | stopped | crashed | standby
    pub state: String,
    pub pid: u32,
    /// Path of the executable this instance runs.
    pub exec: String,
    pub started_ms: u64,
    #[serde(default)]
    pub restarts: u32,
    /// The installed package version this instance runs (empty for WebSocket-served categories).
    #[serde(default)]
    pub build: String,
    /// The git commit the package was built from (`-dirty` when the folder had uncommitted
    /// changes), when the packer recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Whether this version is the plugin's last known good one.
    #[serde(default)]
    pub known_good: bool,
    /// `process` (supervised child) or `websocket` (served by a UI client).
    #[serde(default)]
    pub transport: String,
    /// The plugin (vertical unit, see [`PluginUnit`]) this category belongs to; empty for
    /// WebSocket-served categories.
    #[serde(default)]
    pub plugin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PluginListResult {
    pub plugins: Vec<PluginInfo>,
    /// Every installed plugin as a unit, enabled or disabled.
    #[serde(default)]
    pub units: Vec<PluginUnit>,
}

/// A plugin as the person sees it: one installed package holding a backend, UI packages and
/// shell modules, enabled and disabled as a whole.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PluginUnit {
    /// The plugin id (`[plugin] id` of its plugin.toml).
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// The installed package version it runs (`0.1.0`, or `0.1.0+dev.20261017.101203`).
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    /// The unpacked package the plugin runs from (`<CORTEX_HOME>/plugins-store/<id>/<version>`).
    #[serde(default)]
    pub dir: String,
    /// The backend's category, when it has a backend.
    #[serde(default)]
    pub category: Option<String>,
    /// Ids of its UI packages.
    #[serde(default)]
    pub ui: Vec<String>,
    /// Ids of its Electron shell modules.
    #[serde(default)]
    pub shell: Vec<String>,
    /// Folders of its enabled shell modules (empty while disabled), for the Electron shell.
    #[serde(default)]
    pub shell_dirs: Vec<String>,
    /// The version `frame.plugins.rollback {plugin}` would restore.
    #[serde(default)]
    pub previous_version: Option<String>,
    /// Another plugin serves this one's category: it is installed but not running, and its UI
    /// packages are not served, until `frame.plugins.switch` chooses it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub standby: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProvidersParams {
    pub category: String,
}

/// One installed plugin of a category ([`ProvidersResult`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProviderInfo {
    pub plugin: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    /// It serves the category now.
    pub serving: bool,
    /// The machine range it declares (`[backend.machine]`), when it declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<package::MachineFit>,
    /// This machine is inside the declared range (None: no range declared).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fits_machine: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BundledResult {
    /// The packages folder (empty: the app carries none).
    #[serde(default)]
    pub dir: String,
    pub packages: Vec<BundledPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BundledPackage {
    pub id: String,
    pub version: String,
    /// Absolute path of the `.ccplugin` file.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProvidersResult {
    pub category: String,
    /// The plugin serving it.
    #[serde(default)]
    pub serving: String,
    /// Why that one: `user` (chosen with frame.plugins.switch), `machine` (the one whose declared
    /// range holds this machine's memory), `only` (no alternative installed) or `first`.
    #[serde(default)]
    pub chosen_by: String,
    /// This machine's physical memory, which the declared ranges are compared with.
    #[serde(default)]
    pub memory_bytes: u64,
    pub providers: Vec<ProviderInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwitchParams {
    pub category: String,
    pub plugin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwitchResult {
    pub ok: bool,
    pub category: String,
    #[serde(default)]
    pub from_plugin: String,
    #[serde(default)]
    pub to_plugin: String,
    /// False when the plugin already served the category (nothing was done).
    pub changed: bool,
    /// The backend swap in the category's slot.
    #[serde(default)]
    pub backend: Option<SwapResult>,
    #[serde(default)]
    pub steps: Vec<SwapStep>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginToggleParams {
    /// The plugin id ([`PluginUnit::id`]).
    pub plugin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginToggleResult {
    pub plugin: PluginUnit,
    /// Whether the call changed anything (false when it already was in the requested state).
    pub changed: bool,
    /// What was done (stop/start of the backend, UI packages unserved/served).
    #[serde(default)]
    pub steps: Vec<SwapStep>,
    /// UI package ids no longer served (disable) or served again (enable).
    #[serde(default)]
    pub ui: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstallParams {
    /// Absolute path of the `.ccplugin` file.
    pub path: String,
}

/// Outcome of `frame.plugins.install` and of `frame.plugins.rollback {plugin}`: the backend swap
/// and the UI packages served afterwards, committed together.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageSwapResult {
    pub ok: bool,
    pub plugin: String,
    #[serde(default)]
    pub from_version: String,
    #[serde(default)]
    pub to_version: String,
    /// The backend swap, when the plugin has a backend.
    #[serde(default)]
    pub backend: Option<SwapResult>,
    /// The plugin's UI packages as served after the call (new hashes).
    #[serde(default)]
    pub ui: Vec<UiPlugin>,
    /// unpack, backend swap, UI switch.
    #[serde(default)]
    pub steps: Vec<SwapStep>,
    #[serde(default)]
    pub error: Option<String>,
    /// True when a failure restored the previous version.
    #[serde(default)]
    pub rolled_back: bool,
    pub unit: PluginUnit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SwapParams {
    pub category: String,
}

/// The plugin to roll back, by id (`plugin`) or by its backend's category (`category`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RollbackParams {
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub plugin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwapResult {
    pub ok: bool,
    pub category: String,
    #[serde(default)]
    pub from_version: String,
    #[serde(default)]
    pub to_version: String,
    /// Steps taken with durations, e.g. start, hello, probe, handoff, commit, drain, retire.
    pub steps: Vec<SwapStep>,
    #[serde(default)]
    pub error: Option<String>,
    pub rolled_back: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwapStep {
    pub step: String,
    pub ok: bool,
    pub ms: u64,
    #[serde(default)]
    pub detail: String,
}

/// A UI plugin fills one versioned slot of the workbench.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiPlugin {
    pub id: String,
    pub version: String,
    /// e.g. `workbench.sidebar@1`
    pub slot: String,
    /// URL (served by the frame) of the ES module entry.
    pub entry_url: String,
    #[serde(default)]
    pub css_url: Option<String>,
    /// Content hash of the bundle; changes on every rebuild.
    pub hash: String,
    /// How hosts present it (a tab title, a section heading), from the bundle's manifest.json.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Position among the slot's contributions (lower first), from the bundle's manifest.json.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<f64>,
    /// The plugin (vertical unit) this package belongs to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plugin: String,
}

/// The slot of the window: the UI package (its `entry` is an HTML page) the frame serves at `/ui/`,
/// which loads every other UI package into its slot. A plugin provides it (the workbench).
pub const WINDOW_SLOT: &str = "window@1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UiPluginListResult {
    pub plugins: Vec<UiPlugin>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubscribeParams {
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SubscribeResult {
    /// Every prefix this connection is now subscribed to.
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UiOpenResult {
    /// Whether the Electron shell is running after the call.
    pub running: bool,
    #[serde(default)]
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeepAwakeParams {
    /// Stable key of the holder (e.g. a run id).
    pub holder: String,
    pub hold: bool,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KeepAwakeResult {
    /// Whether the frame holds a power assertion after this call.
    pub asserted: bool,
    /// Number of distinct holders (all connections).
    pub holders: usize,
    /// IOKit assertion id while asserted.
    #[serde(default)]
    pub assertion_id: u32,
}

/// `frame.ui_state.get`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UiStateDocument {
    /// `{ <namespace>: { <key>: value } }` (an empty object until the first put).
    pub document: Value,
    /// Incremented by every put (0 before the first).
    pub rev: u64,
    #[serde(default)]
    pub updated_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiStatePutParams {
    /// The whole document (a JSON object).
    pub document: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UiStatePutResult {
    pub rev: u64,
}

/// Machine facts measured by the frame at startup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Machine {
    pub chip: String,
    pub memory_bytes: u64,
    pub cpu_cores: u32,
    pub performance_cores: u32,
    pub gpu_cores: u32,
    /// Metal `recommendedMaxWorkingSetSize`, when measurable.
    pub gpu_working_set_bytes: u64,
    pub os_version: String,
    #[serde(default)]
    pub extra: Value,
}

/// Plugin folders (`plugin.toml`) and portable `.ccplugin` packages (schema `ccplugin/1`).
///
/// A plugin is one self-contained folder: `plugin.toml`, `backend/` (a Cargo workspace, when it
/// has a backend), `ui/` (one or more npm packages, when it has UI), shell modules, `verify/` and
/// `README.md`. `cortex-pack` compiles it into a `.ccplugin`: a gzip-compressed tar holding
/// `manifest.json` ([`CcPluginManifest`]), the backend executable with its assets, every UI
/// package's built `dist/`, the shell modules and the plugin's data folders. The frame runs
/// plugins only from installed packages, one folder per version.
pub mod package {
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use std::path::{Component, Path};

    /// The schema of `plugin.toml` and of a package's `manifest.json`.
    pub const FORMAT: &str = "ccplugin/1";
    /// The manifest file of a plugin folder.
    pub const PLUGIN_TOML: &str = "plugin.toml";
    /// The manifest file inside a `.ccplugin`.
    pub const PACKAGE_MANIFEST: &str = "manifest.json";
    /// File extension of a package.
    pub const EXTENSION: &str = "ccplugin";

    /// `plugin.toml`.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct PluginToml {
        pub plugin: PluginMeta,
        /// The logic plugin: a supervised process speaking CPP/1.
        #[serde(default)]
        pub backend: Option<BackendSpec>,
        /// UI packages served to the workbench.
        #[serde(default)]
        pub ui: Option<UiSpec>,
        /// Code that runs in the Electron shell's main process.
        #[serde(default)]
        pub shell: Vec<ShellSpec>,
        /// The plugin's data (prompts, texts, skills): packed at the same paths and kept live,
        /// editable, in the home's live folder.
        #[serde(default)]
        pub data: Option<DataSpec>,
        /// Free-form: the plugin's settings keys (documentation).
        #[serde(default)]
        pub settings: Option<Value>,
        /// Verification commands by name, run from the plugin folder.
        #[serde(default)]
        pub verify: Option<Value>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(deny_unknown_fields)]
    pub struct PluginMeta {
        /// The plugin id: the folder name, `[a-z0-9-]`. UI package ids are `<id>.<package>`.
        pub id: String,
        #[serde(default)]
        pub name: String,
        pub version: String,
        #[serde(default)]
        pub description: String,
        #[serde(default)]
        pub license: String,
        #[serde(default)]
        pub platforms: Vec<String>,
        /// The plugin's icon (a file in the folder: .svg, .png or .icns); in a package, `icon.<ext>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub icon: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(deny_unknown_fields)]
    pub struct BackendSpec {
        /// The category it serves (absent only for test-only backends).
        #[serde(default)]
        pub category: Option<String>,
        /// `<category>@<major>`.
        #[serde(default)]
        pub contract: Option<String>,
        /// The Cargo workspace (default `backend`).
        #[serde(default = "default_backend_dir")]
        pub dir: String,
        /// Build command, run in `dir`.
        #[serde(default)]
        pub build: Vec<String>,
        /// The executable the build produces (relative to the plugin folder).
        #[serde(default)]
        pub exec: Option<String>,
        #[serde(default)]
        pub args: Vec<String>,
        /// Files or directories shipped beside the executable (relative to the plugin folder).
        #[serde(default)]
        pub assets: Vec<String>,
        /// The swap policy the plugin declares in its hello (`blue_green` | `drain_first`),
        /// informational: the hello is authoritative.
        #[serde(default)]
        pub swap: Option<String>,
        #[serde(default)]
        pub hello_timeout_ms: Option<u64>,
        #[serde(default)]
        pub probe_timeout_ms: Option<u64>,
        #[serde(default)]
        pub provides: Vec<String>,
        #[serde(default)]
        pub subscribes: Vec<String>,
        #[serde(default)]
        pub publishes: Vec<String>,
        #[serde(default)]
        pub uses: Vec<String>,
        /// Defaults for the plugin's config (`HelloAck.config`); the frame config's
        /// `[plugin.config]` keys override them.
        #[serde(default)]
        pub config: Option<Value>,
        /// The machines this backend is meant for. When several plugins serve one category and
        /// the person chose none, the frame runs the one whose range holds the machine's memory.
        #[serde(default)]
        pub machine: Option<MachineFit>,
    }

    /// `[backend.machine]`: a range of physical memory (`hw.memsize`), both ends inclusive.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
    #[serde(deny_unknown_fields)]
    pub struct MachineFit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub min_memory_bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_memory_bytes: Option<u64>,
    }

    impl MachineFit {
        pub fn holds(&self, memory_bytes: u64) -> bool {
            self.min_memory_bytes.is_none_or(|min| memory_bytes >= min) && self.max_memory_bytes.is_none_or(|max| memory_bytes <= max)
        }
    }

    fn default_backend_dir() -> String {
        "backend".into()
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(deny_unknown_fields)]
    pub struct UiSpec {
        /// The npm root (`npm ci` runs here; its package-lock.json pins every package).
        pub dir: String,
        /// Build command, run in `dir`; builds every package's `dist/`.
        #[serde(default)]
        pub build: Vec<String>,
        #[serde(default)]
        pub package: Vec<UiPackageSpec>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(deny_unknown_fields)]
    pub struct UiPackageSpec {
        /// The npm package folder.
        pub dir: String,
        /// `<plugin id>.<name>`, equal to the built manifest.json's id.
        pub id: String,
        /// The slot it fills, e.g. `pane.terminal@1`.
        pub slot: String,
        /// The build output with manifest.json (default `<dir>/dist`).
        #[serde(default)]
        pub dist: Option<String>,
    }

    impl UiPackageSpec {
        pub fn dist_dir(&self) -> String {
            self.dist.clone().unwrap_or_else(|| format!("{}/dist", self.dir.trim_end_matches('/')))
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(deny_unknown_fields)]
    pub struct ShellSpec {
        /// The module folder (its package.json names the entry: `cortex.shellModule.entry`).
        pub dir: String,
        pub id: String,
        /// The category the module serves over its own frame connection, if any.
        #[serde(default)]
        pub serves: Option<String>,
    }

    /// `[data]`: the folders of the plugin that hold its data, relative to the plugin folder
    /// (`data`, `backend/data`). The package carries them at the same paths; the frame points the
    /// plugin at its folder in the live folder (`<home>/live/plugins/<id>`), where they are edited.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(deny_unknown_fields)]
    pub struct DataSpec {
        #[serde(default)]
        pub dirs: Vec<String>,
    }

    /// Structural check of a path inside a plugin folder or package: relative, no `..`, not empty.
    pub fn safe_relative(value: &str) -> bool {
        let path = Path::new(value);
        !value.is_empty()
            && !path.is_absolute()
            && path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
            && path.components().any(|c| matches!(c, Component::Normal(_)))
    }

    /// Structural check of a plugin id: `[a-z0-9]` words joined by `-`.
    pub fn valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 64
            && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !id.starts_with('-')
            && !id.ends_with('-')
    }

    impl PluginToml {
        /// Structural validation (field presence, id and path syntax, uniqueness).
        pub fn validate(&self) -> Result<(), String> {
            let meta = &self.plugin;
            if !valid_id(&meta.id) {
                return Err(format!("[plugin] id `{}` must be lowercase letters, digits and dashes", meta.id));
            }
            if meta.version.trim().is_empty() {
                return Err("[plugin] version is required".into());
            }
            if let Some(icon) = &meta.icon
                && !safe_relative(icon)
            {
                return Err(format!("[plugin] icon `{icon}` must be a relative path inside the plugin"));
            }
            if let Some(backend) = &self.backend {
                if !safe_relative(&backend.dir) {
                    return Err(format!("[backend] dir `{}` must be a relative path inside the plugin", backend.dir));
                }
                for asset in &backend.assets {
                    if !safe_relative(asset) {
                        return Err(format!("[backend] asset `{asset}` must be a relative path inside the plugin"));
                    }
                }
                let category = backend.category.as_deref().unwrap_or("");
                if category.is_empty() {
                    return Err("[backend] category is required".into());
                }
                let contract = backend.contract.as_deref().unwrap_or("");
                // A family member (`lang-go`) speaks its family's contract (`lang@1`).
                let expected = crate::contracts::family_of(category).unwrap_or(category);
                match contract.split_once('@') {
                    Some((c, major)) if c == expected && !major.is_empty() => {}
                    _ => return Err(format!("[backend] contract `{contract}` must be `{expected}@<major>`")),
                }
                match backend.exec.as_deref() {
                    Some(exec) if safe_relative(exec) => {}
                    Some(exec) => return Err(format!("[backend] exec `{exec}` must be a relative path inside the plugin")),
                    None => return Err("[backend] exec is required".into()),
                }
            }
            let mut ids: Vec<&str> = Vec::new();
            if let Some(ui) = &self.ui {
                if !safe_relative(&ui.dir) {
                    return Err(format!("[ui] dir `{}` must be a relative path inside the plugin", ui.dir));
                }
                for package in &ui.package {
                    if !safe_relative(&package.dir) || !safe_relative(&package.dist_dir()) {
                        return Err(format!("[[ui.package]] `{}`: dir and dist must be relative paths inside the plugin", package.id));
                    }
                    if !package.id.starts_with(&format!("{}.", meta.id)) || package.id.len() <= meta.id.len() + 1 {
                        return Err(format!("[[ui.package]] id `{}` must be `{}.<name>`", package.id, meta.id));
                    }
                    if package.slot.trim().is_empty() {
                        return Err(format!("[[ui.package]] `{}` needs a slot", package.id));
                    }
                    if ids.contains(&package.id.as_str()) {
                        return Err(format!("UI package id `{}` appears twice", package.id));
                    }
                    ids.push(&package.id);
                }
            }
            for shell in &self.shell {
                if !safe_relative(&shell.dir) {
                    return Err(format!("[[shell]] `{}`: dir must be a relative path inside the plugin", shell.id));
                }
                if shell.id.trim().is_empty() || ids.contains(&shell.id.as_str()) {
                    return Err(format!("[[shell]] id `{}` must be non-empty and unique", shell.id));
                }
                ids.push(&shell.id);
            }
            if let Some(data) = &self.data {
                for dir in &data.dirs {
                    if !safe_relative(dir) {
                        return Err(format!("[data] dir `{dir}` must be a relative path inside the plugin"));
                    }
                }
            }
            Ok(())
        }

        /// The backend's runtime config defaults (`[backend.config]`), or `null`.
        pub fn config_defaults(&self) -> Value {
            self.backend.as_ref().and_then(|b| b.config.clone()).unwrap_or(Value::Null)
        }
    }

    /// `manifest.json` inside a `.ccplugin`.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct CcPluginManifest {
        /// [`FORMAT`].
        pub format: String,
        pub plugin: PluginMeta,
        #[serde(default)]
        pub backend: Option<PackedBackend>,
        #[serde(default)]
        pub ui: Vec<PackedUi>,
        #[serde(default)]
        pub shell: Vec<PackedShell>,
        /// The plugin's data folders (`[data] dirs`), packed at these same paths.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub data: Vec<String>,
        pub build: BuildInfo,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct PackedBackend {
        pub category: String,
        pub contract: String,
        /// The executable's path in the package (its assets sit beside it).
        pub exec: String,
        #[serde(default)]
        pub args: Vec<String>,
        /// `[backend.config]` defaults.
        #[serde(default)]
        pub config: Value,
        #[serde(default)]
        pub swap: Option<String>,
        /// Paths of the shipped assets in the package.
        #[serde(default)]
        pub assets: Vec<String>,
        #[serde(default)]
        pub hello_timeout_ms: Option<u64>,
        #[serde(default)]
        pub probe_timeout_ms: Option<u64>,
        /// `[backend.machine]`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub machine: Option<MachineFit>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct PackedUi {
        pub id: String,
        pub slot: String,
        /// The built bundle's folder in the package (holds its manifest.json).
        pub dir: String,
        #[serde(default)]
        pub version: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct PackedShell {
        pub id: String,
        /// The module folder in the package.
        pub dir: String,
        #[serde(default)]
        pub serves: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    pub struct BuildInfo {
        /// Rust target triple the backend was built for (e.g. `aarch64-apple-darwin`).
        pub target: String,
        /// `cortex-pack <version>`.
        pub packer: String,
        pub built_ms: u64,
        /// `rustc --version` of the backend build.
        #[serde(default)]
        pub rustc: String,
        /// `node --version` of the UI build.
        #[serde(default)]
        pub node: String,
        /// The cc-protocol version the packer was built with.
        #[serde(default)]
        pub protocol: String,
        /// The git commit of the plugin folder, when it is in a git repository (`-dirty` when it
        /// had uncommitted changes).
        #[serde(default)]
        pub git_commit: Option<String>,
    }

}

/// [`methods::ROUTE_SET`] parameters: calls of `category` whose `cache_key`, `session_id` or
/// `context.session_id` is `key` go to the client `instance` (`ws-<peer>` from its hello ack).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RouteSetParams {
    pub category: String,
    pub key: String,
    pub instance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RouteClearParams {
    pub category: String,
    pub key: String,
}
