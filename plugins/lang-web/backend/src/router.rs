//! The language-server router: one LSP server process (this executable, `lsp-router`) in front of
//! the HTML server and the CSS server, so the kit's one-server-per-workspace client talks to both.
//! The approach is lang-docker's router, without its Compose answers.
//!
//! - `initialize` goes to both servers (each with its own `initializationOptions` merged over the
//!   client's); the answer merges their capabilities: full-text sync with open/close, `didSave`
//!   when a server wants it, definition / references / hover / highlights when a server has them.
//!   The servers get the router's process id to watch, so they exit when it does.
//! - A document's notifications and requests go to the server of its extension (`.css`, `.scss`,
//!   `.less` to the CSS server, everything else to the HTML server), with the `languageId` the
//!   client chose. Client request ids are replaced by the router's own (and restored on the
//!   answer), so `$/cancelRequest` reaches the right server; a request the document's server has
//!   no provider for is answered with an error at once.
//! - The CSS server's references always include the declaration (it answers them from its document
//!   highlights): references without the declaration are answered from those highlights with the
//!   declaration (DocumentHighlightKind Write) left out.
//! - Requests the servers send (configuration, registrations, progress tokens) go to the client
//!   under router ids; work-done progress tokens are prefixed with the server's index, so the two
//!   servers' tokens never collide in the client's progress tracking.
//! - Diagnostics published without a version (both servers publish unversioned) get the version of
//!   the text the router last sent for that document (the client waits for publications at the
//!   current version).
//! - `shutdown` is answered when both servers have answered it; `exit` is passed on and ends the
//!   router. A server that exits on its own ends the router too (the client restarts it).

use cc_lang_kit::lsp::framing::{encode, read_message};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Index of the HTML server.
pub const HTML: usize = 0;
/// Index of the CSS server.
pub const CSS: usize = 1;

const METHOD_NOT_FOUND: i64 = -32601;

/// One server behind the router.
pub struct ChildIo {
    pub name: String,
    /// Merged over the client's `initializationOptions`.
    pub init_options: Value,
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
}

/// The server of a document (by its URI or path).
pub fn route(uri: &str) -> usize {
    if crate::server::is_stylesheet(uri) { CSS } else { HTML }
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
    /// References without the declaration, from document highlights.
    Highlight { client_id: Value, uri: String },
}

struct ChildState {
    name: String,
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
                let child = uri.as_deref().map(route).unwrap_or(HTML);
                let provider = match method {
                    "textDocument/definition" => Some("definitionProvider"),
                    "textDocument/references" => Some("referencesProvider"),
                    "textDocument/hover" => Some("hoverProvider"),
                    "textDocument/documentHighlight" => Some("documentHighlightProvider"),
                    _ => None,
                };
                if let Some(provider) = provider
                    && let Some(state) = self.children.get(child)
                    && !has_capability(&state.capabilities, provider)
                {
                    let message = format!("{} does not answer {method}", state.name);
                    self.send_client(json!({"jsonrpc": "2.0", "id": id, "error": {"code": METHOD_NOT_FOUND, "message": message}}));
                    return;
                }
                let include_declaration = params.pointer("/context/includeDeclaration").and_then(Value::as_bool).unwrap_or(false);
                if method == "textDocument/references"
                    && child == CSS
                    && !include_declaration
                    && let Some(uri) = uri
                    && self.children.get(child).is_some_and(|c| has_capability(&c.capabilities, "documentHighlightProvider"))
                {
                    let highlight = json!({"textDocument": params["textDocument"], "position": params["position"]});
                    let router_id = self.request_child(child, "textDocument/documentHighlight", highlight, Pending::Highlight { client_id: id.clone(), uri });
                    self.client_requests.insert(key(&id), (child, router_id));
                    return;
                }
                let router_id = self.request_child(child, method, params, Pending::Forward { client_id: id.clone() });
                self.client_requests.insert(key(&id), (child, router_id));
            }
        }
    }

    fn client_notification(&mut self, method: &str, message: Value, params: Value) -> bool {
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
                match method {
                    "textDocument/didClose" => {
                        self.versions.remove(&uri);
                    }
                    "textDocument/didSave" if !self.children.get(child).is_some_and(|c| wants_save(&c.capabilities)) => return true,
                    _ => {
                        if let Some(version) = params.pointer("/textDocument/version").and_then(Value::as_i64) {
                            self.versions.insert(uri, version);
                        }
                    }
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
        for provider in ["definitionProvider", "referencesProvider", "hoverProvider", "documentHighlightProvider"] {
            capabilities.insert(provider.into(), json!(any(provider)));
        }
        Value::Object(capabilities)
    }

    fn on_child(&mut self, child: usize, mut message: Value) {
        let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        match (method, id) {
            // An answer to a request the router sent.
            (None, Some(id)) => {
                let Some((_, purpose)) = self.pending.remove(&key(&id)) else { return };
                match purpose {
                    Pending::Forward { client_id } => {
                        self.client_requests.remove(&key(&client_id));
                        message["id"] = client_id;
                        self.send_client(message);
                    }
                    Pending::Initialize { child } => {
                        let Some(mut init) = self.init.take() else { return };
                        match message.get("error").cloned() {
                            Some(error) => {
                                let name = &self.children[child].name;
                                let text = error.get("message").and_then(Value::as_str).unwrap_or("").to_owned();
                                init.error = Some(json!({"code": error.get("code").cloned().unwrap_or(json!(-32603)), "message": format!("{name}: {text}")}));
                            }
                            None => self.children[child].capabilities = message.pointer("/result/capabilities").cloned().unwrap_or(Value::Null),
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
                                self.reply(init.client_id, json!({"capabilities": capabilities, "serverInfo": {"name": "cc-lang-web lsp-router"}}));
                            }
                        }
                    }
                    Pending::Shutdown => self.shutdown_answered(),
                    Pending::Highlight { client_id, uri } => {
                        self.client_requests.remove(&key(&client_id));
                        if let Some(error) = message.get("error").cloned() {
                            self.send_client(json!({"jsonrpc": "2.0", "id": client_id, "error": error}));
                            return;
                        }
                        let locations: Vec<Value> = message
                            .get("result")
                            .and_then(Value::as_array)
                            .map(|highlights| {
                                highlights
                                    .iter()
                                    // DocumentHighlightKind 3 (Write) marks the declaration.
                                    .filter(|h| h.get("kind").and_then(Value::as_u64) != Some(3))
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
        states.push(ChildState { name: child.name, init_options: child.init_options, tx, capabilities: Value::Null, alive: true });
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

/// `cc-lang-web lsp-router [--version]`: serve LSP on stdio with the installed servers.
pub fn main(args: &[String]) -> i32 {
    let toolchains = cc_lang_kit::install::Toolchains::new(&cc_lang_kit::install::cortex_home());
    let children = match crate::server::children(&toolchains) {
        Ok(children) => children,
        Err(problem) => {
            eprintln!("cc-lang-web lsp-router: {problem}");
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
            eprintln!("cc-lang-web lsp-router: {error}");
            return 1;
        }
    };
    runtime.block_on(async move {
        let mut processes = Vec::new();
        let mut ios = Vec::new();
        let servers = [
            ("vscode-html-language-server", children.html.clone(), crate::server::html_init_options()),
            ("vscode-css-language-server", children.css.clone(), crate::server::css_init_options()),
        ];
        for (name, script, init_options) in servers {
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
                    eprintln!("cc-lang-web lsp-router: cannot start {name}: {error}");
                    return 1;
                }
            };
            let (Some(stdin), Some(stdout)) = (process.stdin.take(), process.stdout.take()) else {
                eprintln!("cc-lang-web lsp-router: {name} has no stdio");
                return 1;
            };
            ios.push(ChildIo { name: name.into(), init_options, reader: Box::new(stdout), writer: Box::new(stdin) });
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
                eprintln!("cc-lang-web lsp-router: {problem}");
                1
            }
        }
    })
}
