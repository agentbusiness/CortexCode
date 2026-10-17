//! A JSON-RPC client of one language server over a pair of byte streams (the server process's
//! stdout and stdin).
//!
//! A reader task routes the server's responses to their requests, answers the requests a server
//! sends its client (configuration, workspace folders, progress tokens, registrations) and keeps
//! what the server publishes on its own in an [`Inbox`]: diagnostics per document, work-done
//! progress in flight and, for servers that report it (`experimental/serverStatus`, rust-analyzer's
//! extension), whether the server is quiescent. Diagnostics the client pulled
//! (`textDocument/diagnostic`) are kept there too ([`LspClient::record_pull`]). Every such event
//! bumps a sequence number that waiters watch.

use super::framing::{ReadError, encode, read_message};
use super::position::{uri_to_path, workspace_case};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

/// JSON-RPC / LSP error codes the client acts on.
pub mod codes {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The document changed while the server computed the answer: ask again.
    pub const CONTENT_MODIFIED: i64 = -32801;
    /// The server cancelled the request (busy, not ready): ask again.
    pub const SERVER_CANCELLED: i64 = -32802;
}

#[derive(Debug, Clone, PartialEq)]
pub enum LspError {
    /// The server's stream closed (it exited or crashed).
    Closed(String),
    /// No answer within the limit (the request was cancelled at the server).
    Timeout(String),
    /// The server answered with an error.
    Response { code: i64, message: String },
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LspError::Closed(reason) => write!(f, "the language server is gone: {reason}"),
            LspError::Timeout(method) => write!(f, "the language server did not answer {method} in time"),
            LspError::Response { code, message } => write!(f, "the language server answered error {code}: {message}"),
        }
    }
}

/// Diagnostics a server published for one document.
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    pub diagnostics: Vec<Value>,
    /// The document version they were computed for, when the server says.
    pub version: Option<i64>,
    /// [`Inbox::seq`] of this publication.
    pub seq: u64,
}

/// What a server reported without being asked.
#[derive(Debug)]
pub struct Inbox {
    /// Latest diagnostics per file.
    pub published: HashMap<PathBuf, Published>,
    /// Latest diagnostics the client pulled per file (`version`: the document version asked about).
    pub pulled: HashMap<PathBuf, Published>,
    /// Bumped by every event (a publication, progress, a status change).
    pub seq: u64,
    /// The server has published diagnostics with a document version (so a publication without one
    /// is not about an open document's current text).
    pub versioned: bool,
    /// Work-done progress begun and not yet ended: token -> title.
    pub progress: HashMap<String, String>,
    /// `experimental/serverStatus`: Some(false) while the server is still loading or analysing the
    /// workspace; None when the server never reported it.
    pub quiescent: Option<bool>,
    /// The server's latest status message, when it gave one.
    pub status_message: Option<String>,
    /// When the server last reported anything (or the client last told it something).
    pub last_activity: Instant,
}

impl Inbox {
    fn new(expect_status: bool) -> Self {
        Self {
            published: HashMap::new(),
            pulled: HashMap::new(),
            seq: 0,
            versioned: false,
            progress: HashMap::new(),
            // A server asked for status reports is loading until it first reports itself quiescent.
            quiescent: expect_status.then_some(false),
            status_message: None,
            last_activity: Instant::now(),
        }
    }

    /// The server says it is working: work-done progress is in flight, or it reported itself not
    /// quiescent.
    pub fn busy(&self) -> bool {
        !self.progress.is_empty() || self.quiescent == Some(false)
    }

    /// The server's analysis is ready for queries. A server that reports quiescence is ready when
    /// it says so (its progress may be a long-running check that queries do not need); for any
    /// other server, when no work-done progress is in flight.
    pub fn ready_for_queries(&self) -> bool {
        match self.quiescent {
            Some(quiescent) => quiescent,
            None => self.progress.is_empty(),
        }
    }
}

/// What the client answers when the server asks.
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    /// `workspace/configuration` answers: each requested section is looked up in this object by
    /// its dotted path (null when absent).
    pub settings: Value,
    /// The `workspace/workspaceFolders` answer.
    pub folders: Value,
    /// The server was asked for `experimental/serverStatus` reports (it counts as not quiescent
    /// until its first report).
    pub expect_status: bool,
    /// The workspace root: published paths under it in another letter case are spelled as the
    /// workspace spells them ([`workspace_case`]).
    pub root: Option<PathBuf>,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspError>>>>>;

pub struct LspClient {
    out: mpsc::UnboundedSender<Vec<u8>>,
    pending: Pending,
    next_id: AtomicI64,
    inbox: Arc<Mutex<Inbox>>,
    events: Arc<watch::Sender<u64>>,
    closed: CancellationToken,
    close_reason: Arc<Mutex<Option<String>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Look up a dotted `section` (`rust-analyzer`, `python.analysis`) in `settings`.
pub fn settings_section(settings: &Value, section: Option<&str>) -> Value {
    let Some(section) = section.filter(|s| !s.is_empty()) else { return settings.clone() };
    let mut current = settings;
    for key in section.split('.') {
        match current.get(key) {
            Some(next) => current = next,
            None => return Value::Null,
        }
    }
    current.clone()
}

/// The answer to a request the server sends its client.
fn answer(method: &str, params: &Value, options: &ClientOptions) -> Result<Value, (i64, String)> {
    match method {
        "workspace/configuration" => {
            let items = params.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            Ok(Value::Array(items.iter().map(|item| settings_section(&options.settings, item.get("section").and_then(Value::as_str))).collect()))
        }
        "workspace/workspaceFolders" => Ok(options.folders.clone()),
        "window/workDoneProgress/create" | "client/registerCapability" | "client/unregisterCapability" | "window/showMessageRequest" => {
            Ok(Value::Null)
        }
        // `workspace/diagnostic/refresh`, `workspace/semanticTokens/refresh`, ...: nothing cached here.
        _ if method.starts_with("workspace/") && method.ends_with("/refresh") => Ok(Value::Null),
        "workspace/applyEdit" => Ok(json!({"applied": false, "failureReason": "CortexCode applies edits through its own tools"})),
        "window/showDocument" => Ok(json!({"success": false})),
        _ => Err((codes::METHOD_NOT_FOUND, format!("CortexCode's client does not handle {method}"))),
    }
}

fn token_key(token: &Value) -> String {
    match token {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

struct Reader {
    out: mpsc::UnboundedSender<Vec<u8>>,
    pending: Pending,
    inbox: Arc<Mutex<Inbox>>,
    events: Arc<watch::Sender<u64>>,
    options: ClientOptions,
}

impl Reader {
    fn bump(&self, inbox: &mut Inbox) {
        inbox.seq += 1;
        inbox.last_activity = Instant::now();
        self.events.send_replace(inbox.seq);
    }

    fn handle(&self, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").filter(|id| !id.is_null());
        match (method, id) {
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { return };
                let Some(waiter) = lock(&self.pending).remove(&id) else { return };
                let outcome = match message.get("error") {
                    Some(error) if !error.is_null() => Err(LspError::Response {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: error.get("message").and_then(Value::as_str).unwrap_or("").to_owned(),
                    }),
                    _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = waiter.send(outcome);
            }
            (Some(method), Some(id)) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let reply = match answer(method, &params, &self.options) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err((code, text)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}}),
                };
                let _ = self.out.send(encode(&reply));
            }
            (Some(method), None) => self.notification(method, message.get("params").unwrap_or(&Value::Null)),
            (None, None) => {}
        }
    }

    fn notification(&self, method: &str, params: &Value) {
        match method {
            "textDocument/publishDiagnostics" => {
                let Some(mut path) = params.get("uri").and_then(Value::as_str).and_then(uri_to_path) else { return };
                if let Some(root) = &self.options.root {
                    path = workspace_case(root, &path);
                }
                let diagnostics = params.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
                let version = params.get("version").and_then(Value::as_i64);
                let mut inbox = lock(&self.inbox);
                let seq = inbox.seq + 1;
                inbox.versioned |= version.is_some();
                inbox.published.insert(path, Published { diagnostics, version, seq });
                self.bump(&mut inbox);
            }
            "$/progress" => {
                let Some(token) = params.get("token") else { return };
                let value = params.get("value").unwrap_or(&Value::Null);
                let mut inbox = lock(&self.inbox);
                match value.get("kind").and_then(Value::as_str) {
                    Some("begin") => {
                        let title = value.get("title").and_then(Value::as_str).unwrap_or("").to_owned();
                        inbox.progress.insert(token_key(token), title);
                    }
                    Some("end") => {
                        inbox.progress.remove(&token_key(token));
                    }
                    Some("report") => {}
                    // Partial-result progress: this client never asks for partial results.
                    _ => return,
                }
                self.bump(&mut inbox);
            }
            "experimental/serverStatus" => {
                let mut inbox = lock(&self.inbox);
                inbox.quiescent = params.get("quiescent").and_then(Value::as_bool);
                inbox.status_message = params.get("message").and_then(Value::as_str).map(str::to_owned);
                self.bump(&mut inbox);
            }
            "window/logMessage" | "window/showMessage" => {
                let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                tracing::debug!(message, "language server says");
            }
            _ => {}
        }
    }
}

impl LspClient {
    /// Start talking to a server whose output is `reader` and input is `writer`.
    pub fn start<R, W>(reader: R, writer: W, options: ClientOptions) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (out, mut outgoing) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let inbox = Arc::new(Mutex::new(Inbox::new(options.expect_status)));
        let (events, _) = watch::channel(0u64);
        let events = Arc::new(events);
        let closed = CancellationToken::new();
        let close_reason = Arc::new(Mutex::new(None));

        let writer_closed = closed.clone();
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(bytes) = outgoing.recv().await {
                if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                    writer_closed.cancel();
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });

        let task = Reader { out: out.clone(), pending: pending.clone(), inbox: inbox.clone(), events: events.clone(), options };
        let reader_closed = closed.clone();
        let reader_reason = close_reason.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let reason = loop {
                tokio::select! {
                    // The writer failed: the server is gone even if its output is still open.
                    _ = reader_closed.cancelled() => break "the server stopped reading its input".to_owned(),
                    next = read_message(&mut reader) => match next {
                        Ok(Some(message)) => task.handle(message),
                        Ok(None) => break "the server closed its output".to_owned(),
                        Err(error) if !error.is_fatal() => tracing::warn!(%error, "skipping a language server message"),
                        Err(ReadError::Io(error)) => break format!("reading from the server failed: {error}"),
                        Err(error) => break error.to_string(),
                    },
                }
            };
            lock(&reader_reason).get_or_insert(reason.clone());
            reader_closed.cancel();
            for (_, waiter) in lock(&task.pending).drain() {
                let _ = waiter.send(Err(LspError::Closed(reason.clone())));
            }
            let mut inbox = lock(&task.inbox);
            task.bump(&mut inbox);
        });

        Arc::new(Self { out, pending, next_id: AtomicI64::new(1), inbox, events, closed, close_reason })
    }

    fn send(&self, message: &Value) -> Result<(), LspError> {
        if self.closed.is_cancelled() {
            return Err(LspError::Closed(self.close_reason()));
        }
        self.out.send(encode(message)).map_err(|_| LspError::Closed(self.close_reason()))
    }

    /// Send a notification.
    pub fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let sent = self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
        lock(&self.inbox).last_activity = Instant::now();
        sent
    }

    /// Send a request and wait for its answer at most `limit`. A request abandoned (timed out, or
    /// its future dropped because the caller cancelled) is cancelled at the server.
    pub async fn request(&self, method: &str, params: Value, limit: Duration) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        let mut guard = Abandon { client: self, id, armed: true };
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        match tokio::time::timeout(limit, rx).await {
            Ok(Ok(outcome)) => {
                guard.armed = false;
                outcome
            }
            Ok(Err(_)) => {
                guard.armed = false;
                Err(LspError::Closed(self.close_reason()))
            }
            Err(_) => Err(LspError::Timeout(method.to_owned())),
        }
    }

    /// The server's unsolicited reports.
    pub fn inbox(&self) -> MutexGuard<'_, Inbox> {
        lock(&self.inbox)
    }

    /// Keep diagnostics pulled for `path` at document `version` (an inbox event, like a
    /// publication).
    pub fn record_pull(&self, path: PathBuf, version: i64, diagnostics: Vec<Value>) {
        let mut inbox = lock(&self.inbox);
        let seq = inbox.seq + 1;
        inbox.pulled.insert(path, Published { diagnostics, version: Some(version), seq });
        inbox.seq = seq;
        inbox.last_activity = Instant::now();
        self.events.send_replace(seq);
    }

    /// A receiver that changes with every inbox event and when the server goes away.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.events.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    /// Why the server's stream closed.
    pub fn close_reason(&self) -> String {
        lock(&self.close_reason).clone().unwrap_or_else(|| "the connection is closed".to_owned())
    }

    /// Ask the server to shut down and exit (`shutdown` then `exit`), waiting at most `limit` for
    /// each step.
    pub async fn shutdown(&self, limit: Duration) {
        if self.is_closed() {
            return;
        }
        let _ = self.request("shutdown", Value::Null, limit).await;
        let _ = self.notify("exit", Value::Null);
        let _ = tokio::time::timeout(limit, self.closed.cancelled()).await;
    }
}

/// Cancels an unanswered request at the server when its waiter goes away.
struct Abandon<'a> {
    client: &'a LspClient,
    id: i64,
    armed: bool,
}

impl Drop for Abandon<'_> {
    fn drop(&mut self) {
        if self.armed {
            lock(&self.client.pending).remove(&self.id);
            let _ = self.client.send(&json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": self.id}}));
        }
    }
}
