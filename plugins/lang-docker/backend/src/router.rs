//! The language-server router: one LSP server process (this executable, `lsp-router`) in front of
//! the Dockerfile server and the Compose server, so the kit's one-server-per-workspace client talks
//! to both.
//!
//! - `initialize` goes to every server (each with its own `initializationOptions` merged over the
//!   client's); the answer merges their capabilities: full-text sync with open/close, `didSave`
//!   when a server wants it, and definition / references / hover when the router can answer them.
//!   The servers get the router's process id to watch, so they exit when it does.
//! - A document's notifications and requests go to the server of its file name (Compose file names
//!   to the Compose server, everything else to the Dockerfile server), `didOpen` with that server's
//!   `languageId`. Client request ids are replaced by the router's own (and restored on the
//!   answer), so `$/cancelRequest` reaches the right server.
//! - Requests the servers send (configuration, progress tokens, registrations) go to the client
//!   under router ids; work-done progress tokens are prefixed with the server's index, so two
//!   servers' tokens never collide in the client's progress tracking.
//! - Diagnostics published without a version get the version of the text the router last sent
//!   for that document (the client waits for publications at the current version).
//! - `textDocument/references` to a server without references but with document highlights is
//!   answered from the highlights (read sites; the write site is the declaration). Compose
//!   definition and references are answered by the router from the Compose structure
//!   ([`crate::compose::occurrences`]): services, volumes, networks, secrets and configs.
//! - `shutdown` is answered when every server has answered it; `exit` is passed on and ends the
//!   router. A server that exits on its own ends the router too (the client restarts it).

use crate::compose;
use cc_lang_kit::lsp::framing::{encode, read_message};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Index of the Dockerfile server.
pub const DOCKERFILE: usize = 0;
/// Index of the Compose server.
pub const COMPOSE: usize = 1;

const METHOD_NOT_FOUND: i64 = -32601;

/// One server behind the router.
pub struct ChildIo {
    pub name: String,
    /// The `languageId` its documents are opened with.
    pub language_id: String,
    /// Merged over the client's `initializationOptions`.
    pub init_options: Value,
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
}

/// Whether a path or URI names a Compose file (`compose*.y[a]ml`, `docker-compose*.y[a]ml`).
pub fn is_compose_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    (name.starts_with("compose") || name.starts_with("docker-compose")) && (name.ends_with(".yaml") || name.ends_with(".yml"))
}

/// The server of a document.
pub fn route(uri: &str) -> usize {
    if is_compose_file(uri) { COMPOSE } else { DOCKERFILE }
}

enum Event {
    Client(Value),
    ClientEnd,
    Child(usize, Value),
    ChildEnd(usize),
}

enum Pending {
    Forward { client_id: Value },
    Initialize { child: usize },
    Shutdown,
    Highlight { client_id: Value, uri: String, include_declaration: bool },
}

struct ChildState {
    name: String,
    language_id: String,
    init_options: Value,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    capabilities: Value,
    alive: bool,
}

struct InitState {
    client_id: Value,
    remaining: usize,
    error: Option<Value>,
}

struct Router {
    out: mpsc::UnboundedSender<Vec<u8>>,
    children: Vec<ChildState>,
    next_id: u64,
    /// Router request id -> (server, what the answer is for).
    pending: HashMap<String, (usize, Pending)>,
    /// Client request id (JSON text) -> (server, router id), for cancellation.
    client_requests: HashMap<String, (usize, String)>,
    /// Router id of a server's request to the client -> (server, the server's id).
    server_requests: HashMap<String, (usize, Value)>,
    init: Option<InitState>,
    shutdown: Option<(Value, usize)>,
    /// Version of the text last sent per document URI.
    versions: HashMap<String, i64>,
    /// Text of open Compose documents (the router answers their definitions and references).
    compose_texts: HashMap<String, String>,
}

fn key(id: &Value) -> String {
    id.to_string()
}

fn has_capability(capabilities: &Value, name: &str) -> bool {
    !matches!(capabilities.get(name), None | Some(Value::Null) | Some(Value::Bool(false)))
}

/// Whether a server's `textDocumentSync` asks for `didSave` (a bare kind does, as the kit reads it).
fn wants_save(capabilities: &Value) -> bool {
    match capabilities.get("textDocumentSync") {
        Some(Value::Number(kind)) => kind.as_u64().unwrap_or(0) != 0,
        Some(Value::Object(options)) => matches!(options.get("save"), Some(Value::Bool(true)) | Some(Value::Object(_))),
        _ => false,
    }
}

/// `overlay`'s keys over `base` (objects), else `overlay` unless it is null.
fn merge_options(base: Option<&Value>, overlay: &Value) -> Value {
    match (base, overlay) {
        (Some(Value::Object(base)), Value::Object(overlay)) => {
            let mut merged = base.clone();
            merged.extend(overlay.clone());
            Value::Object(merged)
        }
        (base, Value::Null) => base.cloned().unwrap_or(Value::Null),
        (_, overlay) => overlay.clone(),
    }
}

/// UTF-16 column of byte `byte` in `line`.
fn utf16_column(line: &str, byte: usize) -> u64 {
    line.get(..byte.min(line.len())).map(|prefix| prefix.encode_utf16().count() as u64).unwrap_or(0)
}

/// Definition or references of the Compose name at `position` (LSP line and UTF-16 character).
pub fn compose_answer(text: &str, uri: &str, method: &str, position: &Value, include_declaration: bool) -> Value {
    let line = position.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
    let character = position.get("character").and_then(Value::as_u64).unwrap_or(0);
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let span = |o: &compose::Occurrence| {
        let line_text = lines.get(o.line).copied().unwrap_or("");
        (utf16_column(line_text, o.start), utf16_column(line_text, o.end))
    };
    let location = |o: &compose::Occurrence| {
        let (start, end) = span(o);
        json!({"uri": uri, "range": {"start": {"line": o.line, "character": start}, "end": {"line": o.line, "character": end}}})
    };
    let occurrences = compose::occurrences(text);
    let Some(at) = occurrences.iter().find(|o| {
        let (start, end) = span(o);
        o.line == line && start <= character && character <= end
    }) else {
        return Value::Null;
    };
    let same = occurrences.iter().filter(|o| o.namespace == at.namespace && o.name == at.name);
    if method == "textDocument/definition" {
        Value::Array(same.filter(|o| o.definition).map(location).collect())
    } else {
        Value::Array(same.filter(|o| include_declaration || !o.definition).map(location).collect())
    }
}

impl Router {
    fn send_client(&self, message: Value) {
        let _ = self.out.send(encode(&message));
    }

    fn send_child(&self, child: usize, message: Value) {
        if let Some(state) = self.children.get(child).filter(|c| c.alive) {
            let _ = state.tx.send(encode(&message));
        }
    }

    fn new_id(&mut self) -> String {
        self.next_id += 1;
        format!("cc-router-{}", self.next_id)
    }

    fn reply(&self, id: Value, result: Value) {
        self.send_client(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn reply_error(&self, id: Value, code: i64, message: String) {
        self.send_client(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }

    fn request_child(&mut self, child: usize, method: &str, params: Value, purpose: Pending) -> String {
        let id = self.new_id();
        self.pending.insert(key(&json!(id)), (child, purpose));
        self.send_child(child, json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn broadcast(&self, message: &Value) {
        for child in 0..self.children.len() {
            self.send_child(child, message.clone());
        }
    }

    /// A message from the client. Returns false when the router is done (`exit`).
    fn on_client(&mut self, message: Value) -> bool {
        let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => self.client_request(&method, id, params),
            (Some(method), None) => return self.client_notification(&method, message, params),
            (None, Some(id)) => {
                if let Some((child, original)) = self.server_requests.remove(&key(&id)) {
                    let mut answer = message.clone();
                    answer["id"] = original;
                    self.send_child(child, answer);
                }
            }
            (None, None) => {}
        }
        true
    }

    fn client_request(&mut self, method: &str, id: Value, params: Value) {
        match method {
            "initialize" => {
                let alive = self.children.iter().filter(|c| c.alive).count();
                self.init = Some(InitState { client_id: id, remaining: alive, error: None });
                for child in 0..self.children.len() {
                    let mut child_params = params.clone();
                    // The servers watch this process (and exit with it), not the client's.
                    if child_params.get("processId").is_some_and(|p| !p.is_null()) {
                        child_params["processId"] = json!(std::process::id());
                    }
                    child_params["initializationOptions"] = merge_options(params.get("initializationOptions"), &self.children[child].init_options);
                    if child_params["initializationOptions"].is_null()
                        && let Some(object) = child_params.as_object_mut()
                    {
                        object.remove("initializationOptions");
                    }
                    self.request_child(child, "initialize", child_params, Pending::Initialize { child });
                }
            }
            "shutdown" => {
                let alive: Vec<usize> = (0..self.children.len()).filter(|c| self.children[*c].alive).collect();
                if alive.is_empty() {
                    self.reply(id, Value::Null);
                    return;
                }
                self.shutdown = Some((id, alive.len()));
                for child in alive {
                    self.request_child(child, "shutdown", Value::Null, Pending::Shutdown);
                }
            }
            _ => {
                let uri = params.pointer("/textDocument/uri").and_then(Value::as_str).map(str::to_owned);
                let child = uri.as_deref().map(route).unwrap_or(DOCKERFILE);
                let capabilities = self.children.get(child).map(|c| c.capabilities.clone()).unwrap_or(Value::Null);
                let include_declaration = params.pointer("/context/includeDeclaration").and_then(Value::as_bool).unwrap_or(false);
                let provider = match method {
                    "textDocument/definition" => Some("definitionProvider"),
                    "textDocument/references" => Some("referencesProvider"),
                    "textDocument/hover" => Some("hoverProvider"),
                    _ => None,
                };
                if let (Some(provider), Some(uri)) = (provider, uri.as_deref())
                    && !has_capability(&capabilities, provider)
                {
                    if child == COMPOSE && provider != "hoverProvider" {
                        let text = self.compose_texts.get(uri).cloned().unwrap_or_default();
                        let answer = compose_answer(&text, uri, method, params.get("position").unwrap_or(&Value::Null), include_declaration);
                        self.reply(id, answer);
                        return;
                    }
                    if provider == "referencesProvider" && has_capability(&capabilities, "documentHighlightProvider") {
                        let highlight = json!({"textDocument": params["textDocument"], "position": params["position"]});
                        let purpose = Pending::Highlight { client_id: id.clone(), uri: uri.to_owned(), include_declaration };
                        let router_id = self.request_child(child, "textDocument/documentHighlight", highlight, purpose);
                        self.client_requests.insert(key(&id), (child, router_id));
                        return;
                    }
                    let name = self.children.get(child).map(|c| c.name.clone()).unwrap_or_default();
                    self.reply_error(id, METHOD_NOT_FOUND, format!("{name} does not answer {method}"));
                    return;
                }
                let router_id = self.request_child(child, method, params, Pending::Forward { client_id: id.clone() });
                self.client_requests.insert(key(&id), (child, router_id));
            }
        }
    }

    fn client_notification(&mut self, method: &str, mut message: Value, params: Value) -> bool {
        match method {
            "exit" => {
                self.broadcast(&message);
                return false;
            }
            "$/cancelRequest" => {
                if let Some(id) = params.get("id")
                    && let Some((child, router_id)) = self.client_requests.get(&key(id)).cloned()
                {
                    self.send_child(child, json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": router_id}}));
                }
            }
            _ if method.starts_with("textDocument/") => {
                let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str).map(str::to_owned) else {
                    self.broadcast(&message);
                    return true;
                };
                let child = route(&uri);
                let version = params.pointer("/textDocument/version").and_then(Value::as_i64);
                match method {
                    "textDocument/didOpen" => {
                        if let Some(state) = self.children.get(child) {
                            message["params"]["textDocument"]["languageId"] = Value::String(state.language_id.clone());
                        }
                        if child == COMPOSE {
                            let text = params.pointer("/textDocument/text").and_then(Value::as_str).unwrap_or("").to_owned();
                            self.compose_texts.insert(uri.clone(), text);
                        }
                    }
                    "textDocument/didChange" => {
                        let whole = params.get("contentChanges").and_then(Value::as_array).and_then(|changes| changes.iter().rev().find(|c| c.get("range").is_none()));
                        if child == COMPOSE
                            && let Some(text) = whole.and_then(|c| c.get("text")).and_then(Value::as_str)
                        {
                            self.compose_texts.insert(uri.clone(), text.to_owned());
                        }
                    }
                    "textDocument/didClose" => {
                        self.versions.remove(&uri);
                        self.compose_texts.remove(&uri);
                    }
                    "textDocument/didSave" if !self.children.get(child).is_some_and(|c| wants_save(&c.capabilities)) => return true,
                    _ => {}
                }
                if let Some(version) = version {
                    self.versions.insert(uri, version);
                }
                self.send_child(child, message);
            }
            _ => self.broadcast(&message),
        }
        true
    }

    fn merged_capabilities(&self) -> Value {
        let mut sync = json!({"openClose": true, "change": 1});
        if self.children.iter().any(|c| wants_save(&c.capabilities)) {
            sync["save"] = json!({"includeText": false});
        }
        let any = |name: &str| self.children.iter().any(|c| has_capability(&c.capabilities, name));
        let mut capabilities = Map::new();
        capabilities.insert("textDocumentSync".into(), sync);
        // Compose definitions and references are answered by the router itself.
        capabilities.insert("definitionProvider".into(), json!(true));
        capabilities.insert("referencesProvider".into(), json!(true));
        capabilities.insert("hoverProvider".into(), json!(any("hoverProvider")));
        capabilities.insert("documentHighlightProvider".into(), json!(any("documentHighlightProvider")));
        Value::Object(capabilities)
    }

    fn on_child(&mut self, child: usize, mut message: Value) {
        let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        match (method, id) {
            // An answer to a request the router sent.
            (None, Some(id)) => {
                let Some((_, purpose)) = self.pending.remove(&key(&id)) else { return };
                let result = message.get("result").cloned().unwrap_or(Value::Null);
                let error = message.get("error").cloned();
                match purpose {
                    Pending::Forward { client_id } => {
                        self.client_requests.remove(&key(&client_id));
                        message["id"] = client_id;
                        self.send_client(message);
                    }
                    Pending::Initialize { child } => {
                        let Some(mut init) = self.init.take() else { return };
                        match error {
                            Some(error) => {
                                let name = &self.children[child].name;
                                let text = error.get("message").and_then(Value::as_str).unwrap_or("").to_owned();
                                init.error = Some(json!({"code": error.get("code").cloned().unwrap_or(json!(-32603)), "message": format!("{name}: {text}")}));
                            }
                            None => self.children[child].capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null),
                        }
                        init.remaining = init.remaining.saturating_sub(1);
                        if init.remaining > 0 {
                            self.init = Some(init);
                            return;
                        }
                        match init.error {
                            Some(error) => self.send_client(json!({"jsonrpc": "2.0", "id": init.client_id, "error": error})),
                            None => {
                                let capabilities = self.merged_capabilities();
                                self.reply(init.client_id, json!({"capabilities": capabilities, "serverInfo": {"name": "cc-lang-docker lsp-router"}}));
                            }
                        }
                    }
                    Pending::Shutdown => self.shutdown_answered(),
                    Pending::Highlight { client_id, uri, include_declaration } => {
                        self.client_requests.remove(&key(&client_id));
                        if let Some(error) = error {
                            self.send_client(json!({"jsonrpc": "2.0", "id": client_id, "error": error}));
                            return;
                        }
                        let locations: Vec<Value> = result
                            .as_array()
                            .map(|highlights| {
                                highlights
                                    .iter()
                                    // DocumentHighlightKind 3 (Write) marks the declaration.
                                    .filter(|h| include_declaration || h.get("kind").and_then(Value::as_u64) != Some(3))
                                    .filter_map(|h| h.get("range").map(|range| json!({"uri": uri, "range": range})))
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.reply(client_id, Value::Array(locations));
                    }
                }
            }
            // A server's request to the client.
            (Some(method), Some(id)) => {
                if method == "window/workDoneProgress/create"
                    && let Some(token) = message.pointer("/params/token").cloned()
                {
                    message["params"]["token"] = namespaced(child, &token);
                }
                let router_id = self.new_id();
                self.server_requests.insert(key(&json!(router_id)), (child, id));
                message["id"] = json!(router_id);
                self.send_client(message);
            }
            (Some(method), None) => match method.as_str() {
                "textDocument/publishDiagnostics" => {
                    if message.pointer("/params/version").is_none_or(Value::is_null)
                        && let Some(uri) = message.pointer("/params/uri").and_then(Value::as_str)
                        && let Some(version) = self.versions.get(uri)
                    {
                        message["params"]["version"] = json!(version);
                    }
                    self.send_client(message);
                }
                "$/progress" => {
                    if let Some(token) = message.pointer("/params/token").cloned() {
                        message["params"]["token"] = namespaced(child, &token);
                    }
                    self.send_client(message);
                }
                "telemetry/event" => {}
                _ => self.send_client(message),
            },
            (None, None) => {}
        }
    }

    fn shutdown_answered(&mut self) {
        if let Some((id, remaining)) = self.shutdown.take() {
            if remaining > 1 {
                self.shutdown = Some((id, remaining - 1));
            } else {
                self.reply(id, Value::Null);
            }
        }
    }

    /// A server ended. Returns false when the router must end too.
    fn child_ended(&mut self, child: usize) -> bool {
        self.children[child].alive = false;
        if self.shutdown.is_some() {
            // Its shutdown answer will not come.
            let waiting = self.pending.values().filter(|(c, p)| *c == child && matches!(p, Pending::Shutdown)).count();
            self.pending.retain(|_, (c, _)| *c != child);
            for _ in 0..waiting {
                self.shutdown_answered();
            }
            return true;
        }
        false
    }
}

fn namespaced(child: usize, token: &Value) -> Value {
    match token {
        Value::String(text) => Value::String(format!("{child}:{text}")),
        other => Value::String(format!("{child}:{other}")),
    }
}

async fn read_into<R: AsyncRead + Unpin>(reader: R, events: mpsc::UnboundedSender<Event>, wrap: impl Fn(Value) -> Event, end: Event) {
    let mut reader = BufReader::new(reader);
    loop {
        match read_message(&mut reader).await {
            Ok(Some(message)) => {
                if events.send(wrap(message)).is_err() {
                    return;
                }
            }
            Ok(None) => break,
            Err(error) if !error.is_fatal() => continue,
            Err(_) => break,
        }
    }
    let _ = events.send(end);
}

fn writer_task<W: AsyncWrite + Unpin + Send + 'static>(mut writer: W) -> (mpsc::UnboundedSender<Vec<u8>>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let handle = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });
    (tx, handle)
}

/// Route between the client on `input` / `output` and `children` until the client sends `exit`,
/// closes its side, or a server ends outside a shutdown (an error).
pub async fn run<R, W>(children: Vec<ChildIo>, input: R, output: W) -> Result<(), String>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
    let (out, out_task) = writer_task(output);
    let mut writers = Vec::new();
    let mut states = Vec::new();
    for (index, child) in children.into_iter().enumerate() {
        let (tx, handle) = writer_task(child.writer);
        writers.push(handle);
        tokio::spawn(read_into(child.reader, events_tx.clone(), move |m| Event::Child(index, m), Event::ChildEnd(index)));
        states.push(ChildState { name: child.name, language_id: child.language_id, init_options: child.init_options, tx, capabilities: Value::Null, alive: true });
    }
    tokio::spawn(read_into(input, events_tx.clone(), Event::Client, Event::ClientEnd));
    drop(events_tx);
    let mut router = Router {
        out,
        children: states,
        next_id: 0,
        pending: HashMap::new(),
        client_requests: HashMap::new(),
        server_requests: HashMap::new(),
        init: None,
        shutdown: None,
        versions: HashMap::new(),
        compose_texts: HashMap::new(),
    };
    let mut outcome = Ok(());
    while let Some(event) = events.recv().await {
        match event {
            Event::Client(message) => {
                if !router.on_client(message) {
                    break;
                }
            }
            Event::ClientEnd => break,
            Event::Child(child, message) => router.on_child(child, message),
            Event::ChildEnd(child) => {
                if !router.child_ended(child) {
                    outcome = Err(format!("{} exited", router.children[child].name));
                    break;
                }
            }
        }
    }
    // Flush what is queued for the client and the servers, then close their streams.
    let Router { out, children, .. } = router;
    drop(out);
    drop(children);
    let _ = out_task.await;
    for writer in writers {
        let _ = writer.await;
    }
    outcome
}

/// `cc-lang-docker lsp-router [--version]`: serve LSP on stdio with the installed servers.
pub fn main(args: &[String]) -> i32 {
    let toolchains = cc_lang_kit::install::Toolchains::new(&cc_lang_kit::install::cortex_home());
    let children = match crate::server::children(&toolchains) {
        Ok(children) => children,
        Err(problem) => {
            eprintln!("cc-lang-docker lsp-router: {problem}");
            return 1;
        }
    };
    if args.iter().any(|a| a == "--version") {
        println!("{}", children.version());
        return 0;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-lang-docker lsp-router: {error}");
            return 1;
        }
    };
    runtime.block_on(async move {
        let mut processes = Vec::new();
        let mut ios = Vec::new();
        let servers = [
            ("docker-langserver", "dockerfile", children.dockerfile.clone(), Value::Null),
            // Diagnostics a short moment after a change instead of the default second.
            ("docker-compose-langserver", "dockercompose", children.compose.clone(), json!({"diagnosticDelay": 200})),
        ];
        for (name, language_id, script, init_options) in servers {
            let spawned = tokio::process::Command::new(&children.node)
                .arg(&script)
                .arg("--stdio")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(true)
                .spawn();
            let mut process = match spawned {
                Ok(process) => process,
                Err(error) => {
                    eprintln!("cc-lang-docker lsp-router: cannot start {name}: {error}");
                    return 1;
                }
            };
            let (Some(stdin), Some(stdout)) = (process.stdin.take(), process.stdout.take()) else {
                eprintln!("cc-lang-docker lsp-router: {name} has no stdio");
                return 1;
            };
            ios.push(ChildIo { name: name.into(), language_id: language_id.into(), init_options, reader: Box::new(stdout), writer: Box::new(stdin) });
            processes.push(process);
        }
        let outcome = run(ios, tokio::io::stdin(), tokio::io::stdout()).await;
        // The servers end on `exit` or when their stdin closes; any that lingers is killed.
        for process in &mut processes {
            if tokio::time::timeout(std::time::Duration::from_secs(2), process.wait()).await.is_err() {
                let _ = process.kill().await;
            }
        }
        match outcome {
            Ok(()) => 0,
            Err(problem) => {
                eprintln!("cc-lang-docker lsp-router: {problem}");
                1
            }
        }
    })
}
