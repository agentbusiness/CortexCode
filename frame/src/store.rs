//! Where the frame's records live: the memory plugin keeps them in its main store (`memory.app.*`
//! for projects and sessions, `memory.journal.*` for each session's journal stream,
//! `memory.settings.*` at `ui.state` for the workbench's slot state). The frame holds them in
//! memory while it runs and writes every change through one outbox, in the order it was made: a
//! journal patch is acknowledged when the store has it. While no memory plugin serves (starting,
//! swapping, disabled), changes wait in the outbox and go out when one serves again.

use cc_protocol::contracts::frame::{JournalPatch, Project, Session, UiStateDocument};
use cc_protocol::contracts::memory::{methods, AppProjectsResult, AppSessionsResult, JournalReadResult};
use cc_protocol::{codes, Connection, RpcError};
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Notify};

/// The memory plugin's category.
pub const MEMORY: &str = "memory";
/// One call to the store at most this long (a journal page, a batch of patches).
const CALL_LIMIT: Duration = Duration::from_secs(60);
/// Journal patches of one session sent in one call at most.
const BATCH: usize = 256;
/// The settings path of the workbench's slot state.
const UI_STATE: &str = "ui.state";

/// The connection to the memory plugin that serves now, if any.
pub struct MemoryLink {
    conn: RwLock<Option<Connection>>,
    /// Notified whenever a memory plugin starts serving.
    serving: Notify,
}

impl MemoryLink {
    pub fn new() -> Arc<MemoryLink> {
        Arc::new(MemoryLink { conn: RwLock::new(None), serving: Notify::new() })
    }

    /// A memory plugin serves on `conn` (its route was set).
    pub fn set(&self, conn: Connection) {
        if let Ok(mut current) = self.conn.write() {
            *current = Some(conn);
        }
        self.serving.notify_waiters();
    }

    /// No memory plugin serves (its route was cleared).
    pub fn clear(&self) {
        if let Ok(mut current) = self.conn.write() {
            *current = None;
        }
    }

    /// One call to the memory plugin. While none serves, or the one that served goes away during the
    /// call, it waits for the next one: the frame's records have no other place.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        loop {
            let serving = self.serving.notified();
            let conn = self.conn.read().ok().and_then(|conn| conn.clone()).filter(|conn| !conn.is_closed());
            let Some(conn) = conn else {
                serving.await;
                continue;
            };
            match conn.call_timeout(method, params.clone(), CALL_LIMIT).await {
                Err(error) if gone(&error) => {
                    tracing::warn!(method, %error.message, "the memory plugin went away during a call; waiting for it");
                    serving.await;
                }
                outcome => return outcome,
            }
        }
    }
}

/// A call that did not reach a serving memory plugin.
fn gone(error: &RpcError) -> bool {
    matches!(error.code.as_str(), codes::CONNECTION_CLOSED | codes::DRAINING)
}

/// One change to write through.
pub enum Op {
    PutProject(Project),
    DeleteProject(String),
    PutSession(Session),
    DeleteSessions(Vec<String>),
    Patch { session_id: String, patch: JournalPatch, ack: oneshot::Sender<Result<(), String>> },
    UiState(UiStateDocument),
    /// Answered once everything sent before it is written.
    Flush(oneshot::Sender<()>),
}

/// The ordered outbox of changes to the store.
#[derive(Clone)]
pub struct Outbox(mpsc::UnboundedSender<Op>);

impl Outbox {
    pub fn new() -> (Outbox, mpsc::UnboundedReceiver<Op>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Outbox(tx), rx)
    }

    pub fn send(&self, op: Op) -> Result<(), RpcError> {
        self.0.send(op).map_err(|_| RpcError::new(codes::UNAVAILABLE, "the store writer stopped: the change was not queued"))
    }

    /// Wait until everything sent so far is written.
    pub async fn flush(&self) -> Result<(), RpcError> {
        let (tx, rx) = oneshot::channel();
        self.send(Op::Flush(tx))?;
        rx.await.map_err(|_| RpcError::new(codes::UNAVAILABLE, "the store writer stopped before changes were durable"))
    }
}

/// Write the outbox through, in order, for as long as the frame runs: consecutive patches of one
/// session go in one call.
pub fn spawn_writer(link: Arc<MemoryLink>, mut rx: mpsc::UnboundedReceiver<Op>) {
    tokio::spawn(async move {
        let mut carried: Option<Op> = None;
        loop {
            let op = match carried.take() {
                Some(op) => op,
                None => match rx.recv().await {
                    Some(op) => op,
                    None => return,
                },
            };
            match op {
                Op::Patch { session_id, patch, ack } => {
                    let mut patches = vec![patch];
                    let mut acks = vec![ack];
                    while patches.len() < BATCH {
                        match rx.try_recv() {
                            Ok(Op::Patch { session_id: next, patch, ack }) if next == session_id => {
                                patches.push(patch);
                                acks.push(ack);
                            }
                            Ok(other) => {
                                carried = Some(other);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    write(&link, methods::JOURNAL_APPEND, json!({"session_id": session_id, "patches": patches})).await;
                    for ack in acks {
                        let _ = ack.send(Ok(()));
                    }
                }
                Op::Flush(done) => {
                    let _ = done.send(());
                }
                other => {
                    let (method, params) = request(other);
                    write(&link, method, params).await;
                }
            }
        }
    });
}

/// Never move past an unacknowledged write: upserts/deletes are idempotent, and journal
/// revisions make an append retried after an ambiguous timeout harmless during replay.
async fn write(link: &MemoryLink, method: &str, params: Value) {
    let mut retry = Duration::from_millis(250);
    loop {
        match link.call(method, params.clone()).await {
            Ok(_) => return,
            Err(error) => {
                tracing::error!(method, %error.message, retry_ms = retry.as_millis(), "store write not acknowledged; retaining it and retrying in order");
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(Duration::from_secs(5));
            }
        }
    }
}

/// The memory call that writes a record change.
fn request(op: Op) -> (&'static str, Value) {
    match op {
        Op::PutProject(project) => (methods::APP_PROJECT_PUT, json!({ "project": project })),
        Op::DeleteProject(id) => (methods::APP_PROJECT_DELETE, json!({ "id": id })),
        Op::PutSession(session) => (methods::APP_SESSION_PUT, json!({ "session": session })),
        Op::DeleteSessions(ids) => (methods::APP_SESSIONS_DELETE, json!({ "ids": ids })),
        Op::UiState(document) => (methods::SETTINGS_SET, json!({ "path": UI_STATE, "value": document, "by": "frame" })),
        Op::Patch { .. } | Op::Flush(_) => unreachable!("written by the writer loop"),
    }
}

/// The records the store holds: projects, sessions and the slot state (None when never saved).
pub async fn load(link: &MemoryLink) -> Result<(Vec<Project>, Vec<Session>, Option<UiStateDocument>), RpcError> {
    let projects: AppProjectsResult = serde_json::from_value(link.call(methods::APP_PROJECTS, json!({})).await?).map_err(|error| RpcError::internal(error.to_string()))?;
    let sessions: AppSessionsResult = serde_json::from_value(link.call(methods::APP_SESSIONS, json!({})).await?).map_err(|error| RpcError::internal(error.to_string()))?;
    let ui_state = match link.call(methods::SETTINGS_GET, json!({ "path": UI_STATE })).await {
        Ok(setting) => setting.get("value").cloned().and_then(|value| serde_json::from_value(value).ok()),
        Err(error) if error.code == codes::NOT_FOUND => None,
        Err(error) => return Err(error),
    };
    Ok((projects.projects, sessions.sessions, ui_state))
}

/// A session's whole journal, in order (empty when it has none).
pub async fn read_journal(link: &MemoryLink, session_id: &str) -> Result<Vec<JournalPatch>, RpcError> {
    let mut patches = Vec::new();
    let mut from = 0u64;
    loop {
        let page: JournalReadResult = serde_json::from_value(link.call(methods::JOURNAL_READ, json!({ "session_id": session_id, "from": from })).await?)
            .map_err(|error| RpcError::internal(error.to_string()))?;
        let done = page.end || page.next <= from;
        patches.extend(page.patches);
        from = page.next;
        if done {
            return Ok(patches);
        }
    }
}
