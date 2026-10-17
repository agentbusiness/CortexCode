//! Language servers per workspace root, and the `lang@1` queries over them.
//!
//! A server starts on the first request for its root (`initialize` with the root's URI as root and
//! only workspace folder, then `initialized` and `workspace/didChangeConfiguration` with the root's
//! settings). How it starts and what it is told can differ per root ([`Launch`], from the launcher).
//! Before every request the documents this client has
//! open are synchronised with the files on disk (the agent edits files between requests):
//! `didOpen` with the file's text, `didChange` with the whole new text when it changed, `didClose`
//! when it was deleted. Diagnostics re-open the requested files (a fresh `didOpen` makes the server
//! publish for them, even when their diagnostics did not change) and save them (`didSave`, which
//! starts on-save checks such as rust-analyzer's `cargo check`), then wait until the server has
//! published for each of them at their current version, reports no work in progress and has been
//! quiet for the settle period, or until the wait ends (`partial`). A server that advertises
//! `diagnosticProvider` (LSP 3.17 pull diagnostics) is also asked (`textDocument/diagnostic`) for
//! the requested files it has never published for: a pulled answer counts like a publication, and a
//! publication at the same version wins over it (servers that push keep being read as before). The
//! client does not declare pull support itself, so servers that adapt to it keep pushing.
//!
//! A server whose process ends is restarted once on the next request; after a second crash the
//! root reports the failure until the servers are reset (`install` does) or the root is reaped. A
//! root unused for the idle period has its server shut down.

use super::client::{ClientOptions, LspClient, LspError, Published, codes as lsp_codes};
use super::position::{Position, from_lsp, line_text, parse_position, path_to_uri, to_lsp, workspace_case};
use cc_protocol::contracts::lang::{Diagnostic, DiagnosticsResult, HoverResult, Location, LocationsResult, PositionParams, name_matches};
use cc_protocol::{BoxFuture, RpcError, codes};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::time::Instant;

/// Documents kept open per server; beyond this the least recently used ones are closed.
pub const MAX_OPEN_DOCUMENTS: usize = 64;
/// Lines of a server's stderr kept for error reports.
const STDERR_LINES: usize = 40;

/// How to run and talk to a language's server.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    /// `rust-analyzer`, `gopls`, ...
    pub name: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Arguments that make the executable print its version (`--version`).
    pub version_args: Vec<String>,
    /// `languageId` of documents by file extension (`rs` -> `rust`). Files matching neither this
    /// nor [`Self::file_names`] are not opened (their diagnostics are still reported when the
    /// server publishes them).
    pub language_ids: Vec<(String, String)>,
    /// `languageId` of documents by file name, before the extension: an exact name (`Dockerfile`,
    /// `go.mod`) or a pattern with `*` (`Dockerfile.*`, `docker-compose.*.yml`; see
    /// [`name_matches`]). Exact names are tried first.
    pub file_names: Vec<(String, String)>,
    /// `initializationOptions` (null: none).
    pub initialization_options: Value,
    /// Answers to `workspace/configuration`, by section.
    pub settings: Value,
    /// `capabilities.experimental` (null: none).
    pub experimental_capabilities: Value,
    /// How long the server must be quiet before its diagnostics count as settled.
    pub settle: Duration,
    /// Default diagnostics wait (`DiagnosticsParams.wait_ms` overrides it).
    pub diagnostics_wait: Duration,
    /// How long a query (definition, references, hover) waits for the server's analysis to be ready
    /// before it asks anyway.
    pub query_wait: Duration,
    /// How long one request may take.
    pub request_timeout: Duration,
    /// How long `initialize` may take.
    pub init_timeout: Duration,
    /// A root unused this long has its server stopped.
    pub idle_stop: Duration,
}

impl ServerSpec {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            args: Vec::new(),
            env: Vec::new(),
            version_args: vec!["--version".into()],
            language_ids: Vec::new(),
            file_names: Vec::new(),
            initialization_options: Value::Null,
            settings: Value::Null,
            experimental_capabilities: Value::Null,
            settle: Duration::from_millis(750),
            diagnostics_wait: Duration::from_secs(60),
            query_wait: Duration::from_secs(60),
            request_timeout: Duration::from_secs(60),
            init_timeout: Duration::from_secs(120),
            idle_stop: Duration::from_secs(20 * 60),
        }
    }

    /// The `languageId` of a file, None when this server does not take it: by exact file name, then
    /// file-name pattern, then extension.
    pub fn language_id(&self, path: &Path) -> Option<&str> {
        let name = path.file_name()?.to_str()?;
        let by_name = self.file_names.iter().find(|(key, _)| key == name).or_else(|| self.file_names.iter().find(|(key, _)| key.contains('*') && name_matches(key, name)));
        if let Some((_, id)) = by_name {
            return Some(id.as_str());
        }
        let extension = path.extension()?.to_str()?;
        self.language_ids.iter().find(|(ext, _)| ext == extension).map(|(_, id)| id.as_str())
    }
}

/// How the server of one workspace root starts, beyond its [`ServerSpec`]: what a pack's
/// `server_launch` says for that root (a per-workspace data directory, compilation database or
/// interpreter). Everything left empty is the spec's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Launch {
    /// Run this instead of the located executable.
    pub program: Option<PathBuf>,
    /// These arguments instead of the spec's.
    pub args: Option<Vec<String>>,
    /// Environment added after the spec's (a name given in both takes this value).
    pub env: Vec<(String, String)>,
    /// This root's `workspace/configuration` answers (and `workspace/didChangeConfiguration`
    /// settings) instead of the spec's.
    pub settings: Option<Value>,
    /// This root's `initializationOptions` instead of the spec's.
    pub initialization_options: Option<Value>,
}

/// A started server's byte streams and, for a process, the process.
pub struct Launched {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    pub stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
    pub child: Option<tokio::process::Child>,
    /// What runs, for reports (`rust-analyzer 1.94.1 at /path`).
    pub description: String,
    /// This root's settings, when not the spec's ([`Launch::settings`]).
    pub settings: Option<Value>,
    /// This root's `initializationOptions`, when not the spec's.
    pub initialization_options: Option<Value>,
}

/// Starts the server for a workspace root.
pub trait Launcher: Send + Sync + 'static {
    fn launch(&self, root: &Path) -> BoxFuture<'static, Result<Launched, String>>;
}

/// Start `exe` as `spec`'s server in `root` (with `launch`'s program, arguments and environment),
/// talking over its stdin and stdout.
pub fn spawn_process(exe: &Path, spec: &ServerSpec, launch: &Launch, root: &Path, description: String) -> Result<Launched, String> {
    let program = launch.program.as_deref().unwrap_or(exe);
    let mut command = tokio::process::Command::new(program);
    command
        .args(launch.args.as_ref().unwrap_or(&spec.args))
        .envs(spec.env.iter().chain(&launch.env).map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| format!("cannot start {}: {e}", program.display()))?;
    let stdin = child.stdin.take().ok_or("the server has no stdin")?;
    let stdout = child.stdout.take().ok_or("the server has no stdout")?;
    let stderr = child.stderr.take().map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>);
    Ok(Launched {
        reader: Box::new(stdout),
        writer: Box::new(stdin),
        stderr,
        child: Some(child),
        description,
        settings: launch.settings.clone(),
        initialization_options: launch.initialization_options.clone(),
    })
}

/// A server state change (published as `<category>.server`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerEvent {
    /// `started` | `stopped` | `failed` (and `installed`, from the pack runtime).
    pub state: String,
    pub server: String,
    pub root: String,
    pub detail: String,
}

pub type EventSink = Arc<dyn Fn(ServerEvent) + Send + Sync>;

/// Adds to or changes a `diagnostics` answer before it is sorted: called with the workspace root,
/// the files the answer covers (relative to the root: the requested files the server took and every
/// file with diagnostics) and the diagnostics.
pub type DiagnosticsHook = Arc<dyn Fn(&Path, &[String], &mut Vec<Diagnostic>) + Send + Sync>;

#[derive(Clone)]
struct Emitter {
    server: String,
    events: EventSink,
}

impl Emitter {
    fn emit(&self, state: &str, root: &Path, detail: impl Into<String>) {
        (self.events)(ServerEvent { state: state.into(), server: self.server.clone(), root: root.to_string_lossy().into_owned(), detail: detail.into() });
    }
}

/// How the server wants documents synchronised (`textDocumentSync`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct SyncCaps {
    open_close: bool,
    /// 0 none, 1 full, 2 incremental (this client always sends the full text, valid for both).
    change: u64,
    /// Some(include_text) when the server wants `didSave`.
    save: Option<bool>,
}

impl SyncCaps {
    fn from_capabilities(capabilities: &Value) -> Self {
        match capabilities.get("textDocumentSync") {
            // A bare kind: open/close and changes of that kind, and saves without text (what the
            // reference client does with a bare kind).
            Some(Value::Number(kind)) => {
                let kind = kind.as_u64().unwrap_or(0);
                Self { open_close: kind != 0, change: kind, save: (kind != 0).then_some(false) }
            }
            Some(options @ Value::Object(_)) => Self {
                open_close: options.get("openClose").and_then(Value::as_bool).unwrap_or(false),
                change: options.get("change").and_then(Value::as_u64).unwrap_or(0),
                save: match options.get("save") {
                    Some(Value::Bool(true)) => Some(false),
                    Some(save @ Value::Object(_)) => Some(save.get("includeText").and_then(Value::as_bool).unwrap_or(false)),
                    _ => None,
                },
            },
            _ => Self { open_close: false, change: 0, save: None },
        }
    }
}

struct Doc {
    version: i64,
    text: String,
    /// [`super::client::Inbox::seq`] when the server was last sent this text.
    synced_seq: u64,
    /// Request counter of the last use (least recently used documents are closed first).
    used: u64,
}

/// A document whose diagnostics a request waits for.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub path: PathBuf,
    pub version: i64,
    pub synced_seq: u64,
}

struct Running {
    client: Arc<LspClient>,
    child: Option<tokio::process::Child>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    sync: SyncCaps,
    /// `diagnosticProvider` was advertised: Some(its `identifier`, null when none).
    pull: Option<Value>,
    description: String,
}

#[derive(Default)]
struct SessionState {
    server: Option<Running>,
    restarts: u32,
    failure: Option<String>,
    docs: HashMap<PathBuf, Doc>,
    tick: u64,
}

struct Session {
    root: PathBuf,
    state: tokio::sync::Mutex<SessionState>,
    last_used: Mutex<Instant>,
}

/// What a request prepared under the session's lock.
struct Prepared {
    client: Arc<LspClient>,
    /// The server takes pulls ([`Running::pull`]).
    pull: Option<Value>,
    targets: Vec<Target>,
    /// Texts of the requested documents as sent to the server.
    texts: HashMap<PathBuf, String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn unavailable(message: impl Into<String>) -> RpcError {
    RpcError::new(codes::UNAVAILABLE, message)
}

fn rpc_error(error: LspError) -> RpcError {
    match error {
        LspError::Closed(_) => unavailable(error.to_string()).retryable(),
        LspError::Timeout(_) => RpcError::new(codes::TIMEOUT, error.to_string()).retryable(),
        LspError::Response { .. } => RpcError::internal(error.to_string()),
    }
}

/// `path` with `.` and `..` folded (no filesystem access).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The workspace root of a request: an existing directory, canonical.
pub fn resolve_root(root: &str) -> Result<PathBuf, RpcError> {
    let path = Path::new(root);
    if !path.is_absolute() {
        return Err(RpcError::invalid_params(format!("root `{root}` is not an absolute path")));
    }
    let canonical = std::fs::canonicalize(path).map_err(|e| RpcError::invalid_params(format!("root `{root}`: {e}")))?;
    if !canonical.is_dir() {
        return Err(RpcError::invalid_params(format!("root `{root}` is not a directory")));
    }
    Ok(canonical)
}

/// A request's file: relative to the root, or absolute.
pub fn resolve_path(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() { normalize(path) } else { normalize(&root.join(path)) }
}

/// How a file is reported: relative to the root when inside it, else absolute.
pub fn display_path(root: &Path, path: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(root) {
        return relative.to_string_lossy().into_owned();
    }
    if let Ok(canonical) = std::fs::canonicalize(path)
        && let Ok(relative) = canonical.strip_prefix(root)
    {
        return relative.to_string_lossy().into_owned();
    }
    path.to_string_lossy().into_owned()
}

fn read_text(path: &Path) -> std::io::Result<String> {
    std::fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// File texts read for position conversion, once per request.
#[derive(Default)]
struct Texts {
    cache: HashMap<PathBuf, Option<String>>,
}

impl Texts {
    fn get(&mut self, path: &Path) -> Option<&str> {
        self.cache.entry(path.to_path_buf()).or_insert_with(|| read_text(path).ok()).as_deref()
    }
}

/// Diagnostics published for `target`'s current text: after it was sent and, for a server that
/// versions its publications, at its version.
fn published_for(published: &Published, target: &Target, versioned: bool) -> bool {
    published.seq > target.synced_seq
        && match published.version {
            Some(version) => version == target.version,
            None => !versioned,
        }
}

/// Diagnostics pulled for `target`'s current text.
fn pulled_for(pulled: &Published, target: &Target) -> bool {
    pulled.seq > target.synced_seq && pulled.version == Some(target.version)
}

/// Wait until every target has diagnostics published (or pulled) after its latest text (at that
/// version, for a server that versions its publications), the server reports no work in progress,
/// and nothing happened for `settle`; Ok(false) when `deadline` comes first.
pub async fn wait_settled(client: &LspClient, targets: &[Target], settle: Duration, deadline: Instant) -> Result<bool, LspError> {
    let mut events = client.subscribe();
    loop {
        events.borrow_and_update();
        if client.is_closed() {
            return Err(LspError::Closed(client.close_reason()));
        }
        let (ready, quiet_at) = {
            let inbox = client.inbox();
            let published = targets.iter().all(|target| {
                inbox.published.get(&target.path).is_some_and(|p| published_for(p, target, inbox.versioned))
                    || inbox.pulled.get(&target.path).is_some_and(|p| pulled_for(p, target))
            });
            (published && !inbox.busy(), inbox.last_activity + settle)
        };
        let now = Instant::now();
        if ready && now >= quiet_at {
            return Ok(true);
        }
        if now >= deadline {
            return Ok(false);
        }
        let until = if ready { quiet_at.min(deadline) } else { deadline };
        let _ = tokio::time::timeout_at(until, events.changed()).await;
    }
}

/// Wait until the server's analysis is ready for queries; Ok(false) when `deadline` comes first.
pub async fn wait_ready(client: &LspClient, deadline: Instant) -> Result<bool, LspError> {
    let mut events = client.subscribe();
    loop {
        events.borrow_and_update();
        if client.is_closed() {
            return Err(LspError::Closed(client.close_reason()));
        }
        if client.inbox().ready_for_queries() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        let _ = tokio::time::timeout_at(deadline, events.changed()).await;
    }
}

fn severity(value: Option<&Value>) -> &'static str {
    match value.and_then(Value::as_u64) {
        Some(2) => "warning",
        Some(3) => "info",
        Some(4) => "hint",
        // 1, and a diagnostic without a severity (the client decides; an unqualified problem is
        // reported as an error).
        _ => "error",
    }
}

/// One LSP diagnostic in CortexCode positions.
pub fn convert_diagnostic(path: String, text: Option<&str>, diagnostic: &Value, server: &str) -> Option<Diagnostic> {
    let range = diagnostic.get("range")?;
    let start = parse_position(range.get("start")?)?;
    let end = parse_position(range.get("end")?)?;
    let (line, column) = from_lsp(text, start);
    let (end_line, end_column) = from_lsp(text, end);
    let code = match diagnostic.get("code") {
        Some(Value::String(code)) => Some(code.clone()),
        Some(Value::Number(code)) => Some(code.to_string()),
        _ => None,
    };
    Some(Diagnostic {
        path,
        line,
        column,
        end_line,
        end_column,
        severity: severity(diagnostic.get("severity")).to_owned(),
        message: diagnostic.get("message").and_then(Value::as_str).unwrap_or("").to_owned(),
        code,
        source: Some(diagnostic.get("source").and_then(Value::as_str).unwrap_or(server).to_owned()),
    })
}

/// The locations of a definition or references answer (`Location`, `Location[]`, `LocationLink[]`).
fn raw_locations(result: &Value) -> Vec<(PathBuf, Position, Position)> {
    let items: Vec<&Value> = match result {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        single => vec![single],
    };
    let mut out = Vec::new();
    for item in items {
        let (uri, range) = match item.get("targetUri") {
            Some(uri) => (uri, item.get("targetSelectionRange").or_else(|| item.get("targetRange"))),
            None => (item.get("uri").unwrap_or(&Value::Null), item.get("range")),
        };
        let Some(path) = uri.as_str().and_then(super::position::uri_to_path) else { continue };
        let Some(range) = range else { continue };
        let (Some(start), Some(end)) = (range.get("start").and_then(parse_position), range.get("end").and_then(parse_position)) else { continue };
        if !out.contains(&(path.clone(), start, end)) {
            out.push((path, start, end));
        }
    }
    out
}

/// A hover answer's text (MarkupContent, MarkedString or a list of them).
pub fn hover_text(result: &Value) -> String {
    fn marked(value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            Value::Object(object) => {
                let text = object.get("value").and_then(Value::as_str).unwrap_or("");
                match object.get("language").and_then(Value::as_str) {
                    Some(language) => format!("```{language}\n{text}\n```"),
                    None => text.to_owned(),
                }
            }
            _ => String::new(),
        }
    }
    match result.get("contents") {
        Some(Value::Array(items)) => items.iter().map(marked).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n"),
        Some(contents) => marked(contents),
        None => String::new(),
    }
}

/// Ask the server for `target`'s diagnostics (`textDocument/diagnostic`) and keep a full report
/// in the inbox; asked again (up to three times) when the server cancelled or the content changed.
/// Anything else leaves the target to publications.
async fn pull(client: Arc<LspClient>, target: Target, identifier: Value, limit: Duration, settle: Duration) {
    let Some(uri) = path_to_uri(&target.path) else { return };
    let mut params = json!({"textDocument": {"uri": uri}});
    if identifier.is_string() {
        params["identifier"] = identifier;
    }
    for attempt in 0..4 {
        match client.request("textDocument/diagnostic", params.clone(), limit).await {
            Ok(report) => {
                if report.get("kind").and_then(Value::as_str) == Some("full") {
                    let items = report.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
                    client.record_pull(target.path, target.version, items);
                }
                return;
            }
            Err(LspError::Response { code, .. }) if (code == lsp_codes::CONTENT_MODIFIED || code == lsp_codes::SERVER_CANCELLED) && attempt < 3 => {
                let mut events = client.subscribe();
                events.borrow_and_update();
                let _ = tokio::time::timeout(settle, events.changed()).await;
            }
            Err(_) => return,
        }
    }
}

fn initialize_params(root: &Path, root_uri: &str, folders: &Value, spec: &ServerSpec, initialization_options: &Value) -> Value {
    let mut capabilities = json!({
        "general": {"positionEncodings": ["utf-16"]},
        "window": {"workDoneProgress": true},
        "workspace": {"configuration": true, "workspaceFolders": true},
        "textDocument": {
            "synchronization": {"dynamicRegistration": false, "didSave": true, "willSave": false, "willSaveWaitUntil": false},
            "publishDiagnostics": {"relatedInformation": false, "versionSupport": true, "codeDescriptionSupport": false, "dataSupport": false},
            "definition": {"dynamicRegistration": false, "linkSupport": true},
            "references": {"dynamicRegistration": false},
            "hover": {"dynamicRegistration": false, "contentFormat": ["markdown", "plaintext"]}
        }
    });
    if !spec.experimental_capabilities.is_null() {
        capabilities["experimental"] = spec.experimental_capabilities.clone();
    }
    let mut params = json!({
        "processId": std::process::id(),
        "clientInfo": {"name": "CortexCode", "version": env!("CARGO_PKG_VERSION")},
        "rootPath": root.to_string_lossy(),
        "rootUri": root_uri,
        "workspaceFolders": folders,
        "capabilities": capabilities,
    });
    if !initialization_options.is_null() {
        params["initializationOptions"] = initialization_options.clone();
    }
    params
}

/// Language servers by workspace root.
pub struct Servers {
    spec: ServerSpec,
    launcher: Arc<dyn Launcher>,
    emitter: Emitter,
    sessions: Mutex<HashMap<PathBuf, Arc<Session>>>,
    annotate: Option<DiagnosticsHook>,
}

impl Servers {
    pub fn new(spec: ServerSpec, launcher: Arc<dyn Launcher>, events: EventSink) -> Self {
        let emitter = Emitter { server: spec.name.clone(), events };
        Self { spec, launcher, emitter, sessions: Mutex::new(HashMap::new()), annotate: None }
    }

    /// Run `hook` on every `diagnostics` answer ([`DiagnosticsHook`]).
    pub fn with_diagnostics_hook(mut self, hook: DiagnosticsHook) -> Self {
        self.annotate = Some(hook);
        self
    }

    pub fn spec(&self) -> &ServerSpec {
        &self.spec
    }

    /// Roots with a session (running, crashed or failed).
    pub fn roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = lock(&self.sessions).keys().cloned().collect();
        roots.sort();
        roots
    }

    fn emit(&self, state: &str, root: &Path, detail: impl Into<String>) {
        self.emitter.emit(state, root, detail);
    }

    fn session(&self, root: &Path) -> Arc<Session> {
        let session = lock(&self.sessions)
            .entry(root.to_path_buf())
            .or_insert_with(|| Arc::new(Session { root: root.to_path_buf(), state: tokio::sync::Mutex::new(SessionState::default()), last_used: Mutex::new(Instant::now()) }))
            .clone();
        *lock(&session.last_used) = Instant::now();
        session
    }

    async fn start(&self, root: &Path) -> Result<Running, RpcError> {
        let launched = self.launcher.launch(root).await.map_err(|problem| unavailable(format!("{} is not available: {problem}", self.spec.name)))?;
        let Launched { reader, writer, stderr, mut child, description, settings, initialization_options } = launched;
        let settings = settings.unwrap_or_else(|| self.spec.settings.clone());
        let initialization_options = initialization_options.unwrap_or_else(|| self.spec.initialization_options.clone());
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = stderr {
            let tail = tail.clone();
            let name = self.spec.name.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(server = %name, "{line}");
                    let mut tail = lock(&tail);
                    if tail.len() == STDERR_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }
        let root_uri = path_to_uri(root).ok_or_else(|| RpcError::invalid_params(format!("{} has no file URI", root.display())))?;
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.to_string_lossy().into_owned());
        let folders = json!([{"uri": root_uri, "name": name}]);
        let expect_status = self.spec.experimental_capabilities.get("serverStatusNotification") == Some(&Value::Bool(true));
        let options = ClientOptions { settings: settings.clone(), folders: folders.clone(), expect_status, root: Some(root.to_path_buf()) };
        let client = LspClient::start(reader, writer, options);
        let params = initialize_params(root, &root_uri, &folders, &self.spec, &initialization_options);
        match client.request("initialize", params, self.spec.init_timeout).await {
            Ok(result) => {
                let capabilities = result.get("capabilities").unwrap_or(&Value::Null);
                let sync = SyncCaps::from_capabilities(capabilities);
                let pull = match capabilities.get("diagnosticProvider") {
                    None | Some(Value::Null) | Some(Value::Bool(false)) => None,
                    Some(provider) => Some(provider.get("identifier").cloned().unwrap_or(Value::Null)),
                };
                client.notify("initialized", json!({})).map_err(rpc_error)?;
                // Servers that only read pushed settings (and those that pull them again when told
                // they changed) get the root's settings now.
                if !settings.is_null() {
                    client.notify("workspace/didChangeConfiguration", json!({"settings": settings})).map_err(rpc_error)?;
                }
                Ok(Running { client, child, stderr: tail, sync, pull, description })
            }
            Err(error) => {
                if let Some(child) = child.as_mut() {
                    let _ = child.start_kill();
                }
                let stderr = lock(&tail).iter().cloned().collect::<Vec<_>>().join("\n");
                let detail = if stderr.is_empty() { String::new() } else { format!("; stderr:\n{stderr}") };
                Err(unavailable(format!("{} did not initialize in {}: {error}{detail}", self.spec.name, root.display())))
            }
        }
    }

    /// The session's client, starting the server if needed and restarting a crashed one once.
    async fn ensure_running(&self, root: &Path, state: &mut SessionState) -> Result<Arc<LspClient>, RpcError> {
        if let Some(running) = &state.server {
            if !running.client.is_closed() {
                return Ok(running.client.clone());
            }
            let stderr = lock(&running.stderr).iter().rev().take(10).rev().cloned().collect::<Vec<_>>().join("\n");
            let reason = if stderr.is_empty() { running.client.close_reason() } else { format!("{}; stderr:\n{stderr}", running.client.close_reason()) };
            if let Some(mut gone) = state.server.take()
                && let Some(child) = gone.child.as_mut()
            {
                let _ = child.start_kill();
            }
            state.docs.clear();
            if state.restarts >= 1 {
                let message = format!("{} crashed again in {} and is not restarted: {reason}", self.spec.name, root.display());
                state.failure = Some(message.clone());
                self.emit("failed", root, message.clone());
                return Err(unavailable(message));
            }
            state.restarts += 1;
            self.emit("failed", root, format!("crashed, restarting: {reason}"));
        }
        if let Some(failure) = &state.failure {
            return Err(unavailable(failure.clone()));
        }
        let running = self.start(root).await?;
        let client = running.client.clone();
        self.emit("started", root, running.description.clone());
        state.server = Some(running);
        Ok(client)
    }

    /// Bring one document in line with the file on disk. Returns the document as sent when the
    /// server tracks it (None: the server does not take this file, or it does not exist).
    fn sync_doc(&self, client: &LspClient, sync: SyncCaps, state: &mut SessionState, path: &Path, reopen: bool, must_exist: bool) -> Result<Option<Target>, RpcError> {
        let tick = state.tick;
        let Some(language_id) = self.spec.language_id(path).filter(|_| sync.open_close) else {
            if must_exist && !path.is_file() {
                return Err(RpcError::not_found(format!("{} does not exist", path.display())));
            }
            return Ok(None);
        };
        let uri = path_to_uri(path).ok_or_else(|| RpcError::invalid_params(format!("{} has no file URI", path.display())))?;
        let text = match read_text(path) {
            Ok(text) => text,
            Err(error) => {
                if state.docs.remove(path).is_some() {
                    client.notify("textDocument/didClose", json!({"textDocument": {"uri": uri}})).map_err(rpc_error)?;
                }
                if must_exist {
                    return Err(RpcError::not_found(format!("cannot read {}: {error}", path.display())));
                }
                return Ok(None);
            }
        };
        let seq = client.inbox().seq;
        let next_version = state.docs.get(path).map(|doc| doc.version + 1).unwrap_or(1);
        match state.docs.get_mut(path) {
            Some(doc) if !reopen && doc.text == text => doc.used = tick,
            // A server that takes no changes gets the new text by a close and a fresh open.
            Some(doc) if !reopen && sync.change != 0 => {
                client
                    .notify("textDocument/didChange", json!({"textDocument": {"uri": uri, "version": next_version}, "contentChanges": [{"text": text}]}))
                    .map_err(rpc_error)?;
                *doc = Doc { version: next_version, text, synced_seq: seq, used: tick };
            }
            existing => {
                if existing.is_some() {
                    client.notify("textDocument/didClose", json!({"textDocument": {"uri": uri}})).map_err(rpc_error)?;
                }
                client
                    .notify("textDocument/didOpen", json!({"textDocument": {"uri": uri, "languageId": language_id, "version": next_version, "text": text}}))
                    .map_err(rpc_error)?;
                state.docs.insert(path.to_path_buf(), Doc { version: next_version, text, synced_seq: seq, used: tick });
            }
        }
        let doc = &state.docs[path];
        Ok(Some(Target { path: path.to_path_buf(), version: doc.version, synced_seq: doc.synced_seq }))
    }

    /// Start (or restart) the root's server and synchronise its documents for a request about
    /// `requested`. `fresh`: re-open and save the requested files (diagnostics).
    async fn prepare(&self, root: &Path, requested: &[PathBuf], fresh: bool) -> Result<Prepared, RpcError> {
        let session = self.session(root);
        let mut state = session.state.lock().await;
        let client = self.ensure_running(&session.root, &mut state).await?;
        self.sync_requested(&session, state, client, requested, fresh)
    }

    /// [`Self::prepare`] without waiting for anything: None when the root has no running server
    /// or another request holds it (starting it, synchronising); the documents are synchronised
    /// but not re-checked.
    fn prepare_now(&self, root: &Path, requested: &[PathBuf]) -> Result<Option<Prepared>, RpcError> {
        let Some(session) = lock(&self.sessions).get(root).cloned() else { return Ok(None) };
        let Ok(state) = session.state.try_lock() else { return Ok(None) };
        let Some(client) = state.server.as_ref().map(|running| running.client.clone()).filter(|client| !client.is_closed()) else { return Ok(None) };
        *lock(&session.last_used) = Instant::now();
        self.sync_requested(&session, state, client, requested, false).map(Some)
    }

    /// Whether `root` has a server running or being started (never waits).
    pub fn is_running(&self, root: &str) -> bool {
        let Ok(root) = resolve_root(root) else { return false };
        let Some(session) = lock(&self.sessions).get(&root).cloned() else { return false };
        match session.state.try_lock() {
            Ok(state) => state.server.as_ref().is_some_and(|running| !running.client.is_closed()),
            Err(_) => true,
        }
    }

    /// Start `root`'s server (if needed) and open `paths`, for a later request to find it running.
    pub async fn start_for(&self, root: &str, paths: &[String]) -> Result<(), RpcError> {
        let root = resolve_root(root)?;
        let requested: Vec<PathBuf> = paths.iter().map(|path| resolve_path(&root, path)).collect();
        self.prepare(&root, &requested, false).await.map(|_| ())
    }

    fn sync_requested(
        &self,
        session: &Session,
        mut state: tokio::sync::MutexGuard<'_, SessionState>,
        client: Arc<LspClient>,
        requested: &[PathBuf],
        fresh: bool,
    ) -> Result<Prepared, RpcError> {
        let (sync, pull) = state.server.as_ref().map(|running| (running.sync, running.pull.clone())).ok_or_else(|| unavailable("the server is not running"))?;
        state.tick += 1;
        let open: Vec<PathBuf> = state.docs.keys().filter(|path| !requested.contains(path)).cloned().collect();
        for path in open {
            self.sync_doc(&client, sync, &mut state, &path, false, false)?;
        }
        let mut targets = Vec::new();
        for path in requested {
            if let Some(target) = self.sync_doc(&client, sync, &mut state, path, fresh, !fresh)? {
                targets.push(target);
            }
        }
        if fresh && let Some(include_text) = sync.save {
            for target in &targets {
                let uri = path_to_uri(&target.path).unwrap_or_default();
                let mut params = json!({"textDocument": {"uri": uri}});
                if include_text {
                    params["text"] = Value::String(state.docs[&target.path].text.clone());
                }
                client.notify("textDocument/didSave", params).map_err(rpc_error)?;
            }
        }
        // Keep the open set bounded: close the least recently used documents not in this request.
        if state.docs.len() > MAX_OPEN_DOCUMENTS {
            let mut idle: Vec<(u64, PathBuf)> = state.docs.iter().filter(|(path, _)| !requested.contains(path)).map(|(path, doc)| (doc.used, path.clone())).collect();
            idle.sort();
            let excess = state.docs.len() - MAX_OPEN_DOCUMENTS;
            for (_, path) in idle.into_iter().take(excess) {
                state.docs.remove(&path);
                if let Some(uri) = path_to_uri(&path) {
                    client.notify("textDocument/didClose", json!({"textDocument": {"uri": uri}})).map_err(rpc_error)?;
                }
            }
        }
        let texts = requested.iter().filter_map(|path| state.docs.get(path).map(|doc| (path.clone(), doc.text.clone()))).collect();
        drop(state);
        *lock(&session.last_used) = Instant::now();
        Ok(Prepared { client, pull, targets, texts })
    }

    /// `lang.diagnostics`.
    /// A zero `wait` takes what the server has now ([`Self::prepare_now`]): nothing (partial) when
    /// no server runs for the root.
    pub async fn diagnostics(&self, root: &str, paths: &[String], wait: Option<Duration>) -> Result<DiagnosticsResult, RpcError> {
        let root = resolve_root(root)?;
        let requested: Vec<PathBuf> = paths.iter().map(|path| resolve_path(&root, path)).collect();
        let now = wait == Some(Duration::ZERO);
        let prepared = if now {
            match self.prepare_now(&root, &requested)? {
                Some(prepared) => prepared,
                None => return Ok(DiagnosticsResult { server: self.spec.name.clone(), diagnostics: Vec::new(), partial: true }),
            }
        } else {
            self.prepare(&root, &requested, true).await?
        };
        let deadline = Instant::now() + wait.unwrap_or(self.spec.diagnostics_wait);
        // Pull (servers with `diagnosticProvider`) the files the server never published for; the
        // pulls end with this request (a dropped request is cancelled at the server).
        let mut pulls = tokio::task::JoinSet::new();
        if !now && let Some(identifier) = &prepared.pull {
            let unpublished: Vec<Target> = {
                let inbox = prepared.client.inbox();
                prepared.targets.iter().filter(|target| !inbox.published.contains_key(&target.path)).cloned().collect()
            };
            for target in unpublished {
                pulls.spawn(pull(prepared.client.clone(), target, identifier.clone(), self.spec.request_timeout, self.spec.settle));
            }
        }
        let settled = wait_settled(&prepared.client, &prepared.targets, self.spec.settle, deadline).await;
        pulls.abort_all();
        let settled = settled.map_err(rpc_error)?;
        let published: Vec<(PathBuf, Vec<Value>)> = {
            let inbox = prepared.client.inbox();
            let mut found: HashMap<PathBuf, Vec<Value>> = inbox
                .published
                .iter()
                .filter(|(path, _)| requested.is_empty() || requested.contains(path))
                .map(|(path, published)| (path.clone(), published.diagnostics.clone()))
                .collect();
            // A target without a publication for its current text takes what was pulled for it.
            for target in &prepared.targets {
                let fresh = inbox.published.get(&target.path).is_some_and(|p| published_for(p, target, inbox.versioned));
                if !fresh && let Some(pulled) = inbox.pulled.get(&target.path).filter(|p| pulled_for(p, target)) {
                    found.insert(target.path.clone(), pulled.diagnostics.clone());
                }
            }
            found.into_iter().filter(|(_, items)| !items.is_empty()).collect()
        };
        let mut covered: Vec<String> = prepared.targets.iter().map(|target| display_path(&root, &target.path)).collect();
        let mut texts = Texts::default();
        let mut diagnostics = Vec::new();
        for (path, items) in published {
            let display = display_path(&root, &path);
            let text = match prepared.texts.get(&path) {
                Some(text) => Some(text.as_str()),
                None => texts.get(&path),
            };
            diagnostics.extend(items.iter().filter_map(|item| convert_diagnostic(display.clone(), text, item, &self.spec.name)));
            if !covered.contains(&display) {
                covered.push(display);
            }
        }
        if let Some(annotate) = &self.annotate {
            covered.sort();
            annotate(&root, &covered, &mut diagnostics);
        }
        diagnostics.sort_by(|a, b| (&a.path, a.line, a.column, &a.message).cmp(&(&b.path, b.line, b.column, &b.message)));
        Ok(DiagnosticsResult { server: self.spec.name.clone(), diagnostics, partial: !settled })
    }

    /// A position query: synchronise, wait for the analysis to be ready, ask (again while the server
    /// says the content changed or it cancelled).
    async fn query(&self, params: &PositionParams, method: &str, extra: Value) -> Result<(PathBuf, Value), RpcError> {
        let root = resolve_root(&params.root)?;
        let path = resolve_path(&root, &params.path);
        let prepared = self.prepare(&root, std::slice::from_ref(&path), false).await?;
        let text = prepared.texts.get(&path).cloned().ok_or_else(|| unavailable(format!("{} does not take {}", self.spec.name, params.path)))?;
        let deadline = Instant::now() + self.spec.query_wait;
        wait_ready(&prepared.client, deadline).await.map_err(rpc_error)?;
        let position = to_lsp(&text, params.line, params.column);
        let mut request = json!({"textDocument": {"uri": path_to_uri(&path)}, "position": position});
        if let (Some(request), Value::Object(extra)) = (request.as_object_mut(), extra) {
            request.extend(extra);
        }
        let mut attempt = 0;
        loop {
            match prepared.client.request(method, request.clone(), self.spec.request_timeout).await {
                Ok(result) => return Ok((root, result)),
                Err(LspError::Response { code, .. }) if (code == lsp_codes::CONTENT_MODIFIED || code == lsp_codes::SERVER_CANCELLED) && attempt < 3 => {
                    attempt += 1;
                    // Ask again after the server reports something (or one settle period).
                    let mut events = prepared.client.subscribe();
                    events.borrow_and_update();
                    let _ = tokio::time::timeout(self.spec.settle, events.changed()).await;
                }
                Err(error) => return Err(rpc_error(error)),
            }
        }
    }

    fn locations(root: &Path, result: &Value) -> LocationsResult {
        let mut texts = Texts::default();
        let locations = raw_locations(result)
            .into_iter()
            .map(|(path, start, end)| {
                let path = workspace_case(root, &path);
                let text = texts.get(&path);
                let (line, column) = from_lsp(text, start);
                let (end_line, end_column) = from_lsp(text, end);
                let line_text = text.and_then(|t| line_text(t, start.line as usize)).map(str::to_owned);
                Location { path: display_path(root, &path), line, column, end_line, end_column, line_text }
            })
            .collect();
        LocationsResult { locations }
    }

    /// `lang.definition`.
    pub async fn definition(&self, params: &PositionParams) -> Result<LocationsResult, RpcError> {
        let (root, result) = self.query(params, "textDocument/definition", Value::Null).await?;
        Ok(Self::locations(&root, &result))
    }

    /// `lang.references`: where the symbol is used (its declaration not included).
    pub async fn references(&self, params: &PositionParams) -> Result<LocationsResult, RpcError> {
        let (root, result) = self.query(params, "textDocument/references", json!({"context": {"includeDeclaration": false}})).await?;
        Ok(Self::locations(&root, &result))
    }

    /// `lang.hover`.
    pub async fn hover(&self, params: &PositionParams) -> Result<HoverResult, RpcError> {
        let (_, result) = self.query(params, "textDocument/hover", Value::Null).await?;
        Ok(HoverResult { text: hover_text(&result) })
    }

    /// Stop the servers of roots unused for the idle period (as of `now`).
    pub async fn reap_idle(&self, now: Instant) {
        let idle: Vec<Arc<Session>> = {
            let mut sessions = lock(&self.sessions);
            let roots: Vec<PathBuf> = sessions
                .iter()
                .filter(|(_, session)| now.saturating_duration_since(*lock(&session.last_used)) >= self.spec.idle_stop)
                .map(|(root, _)| root.clone())
                .collect();
            roots.iter().filter_map(|root| sessions.remove(root)).collect()
        };
        for session in idle {
            stop(&self.emitter, session, "idle").await;
        }
    }

    /// Stop every server (plugin shutdown, or after an install).
    pub async fn stop_all(&self, why: &str) {
        let all: Vec<Arc<Session>> = lock(&self.sessions).drain().map(|(_, session)| session).collect();
        let mut stops = tokio::task::JoinSet::new();
        for session in all {
            let emitter = self.emitter.clone();
            let why = why.to_owned();
            stops.spawn(async move { stop(&emitter, session, &why).await });
        }
        while stops.join_next().await.is_some() {}
    }
}

/// Shut a session's server down (`shutdown`, `exit`, then the process is killed if it lingers).
async fn stop(emitter: &Emitter, session: Arc<Session>, why: &str) {
    let mut state = session.state.lock().await;
    state.docs.clear();
    if let Some(mut running) = state.server.take() {
        running.client.shutdown(Duration::from_secs(3)).await;
        if let Some(child) = running.child.as_mut()
            && tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err()
        {
            let _ = child.kill().await;
        }
        emitter.emit("stopped", &session.root, why);
    }
}
