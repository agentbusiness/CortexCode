//! Where the frame's records live: the memory plugin keeps projects and sessions (`memory.app.*`)
//! and the workbench's slot state (`memory.settings.*` at `ui.state`) in its main store. The frame
//! holds them in memory while it runs and writes every change through one outbox, in the order it
//! was made. While no memory plugin serves (starting, swapping, disabled), changes wait in the
//! outbox and go out when one serves again. Session journals are the frame's own files
//! ([`crate::journal`]); the store's journal streams are read here only to move them into files.

use cc_protocol::contracts::frame::{JournalPatch, Project, ProjectDeleted, Session, UiStateDocument};
use cc_protocol::contracts::records::{methods, AppDeletedResult, AppProjectsResult, AppSessionsResult, JournalReadResult};
use cc_protocol::{codes, Connection, RpcError};
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Notify};

/// The memory plugin's category.
pub const MEMORY: &str = "memory";
/// One call to the store at most this long (a journal page, a record).
const CALL_LIMIT: Duration = Duration::from_secs(60);
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

/// Write the outbox through, in order, for as long as the frame runs.
pub fn spawn_writer(link: Arc<MemoryLink>, mut rx: mpsc::UnboundedReceiver<Op>) {
    tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            match op {
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

/// Never move past an unacknowledged write: upserts and deletes are idempotent, so a write
/// retried after an ambiguous timeout is harmless.
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
        Op::Flush(_) => unreachable!("answered by the writer loop"),
    }
}

/// The records the store holds: projects, sessions, the slot state (None when never saved) and the
/// projects deleted (none from a memory plugin that does not keep them yet).
pub async fn load(link: &MemoryLink) -> Result<(Vec<Project>, Vec<Session>, Option<UiStateDocument>, Vec<ProjectDeleted>), RpcError> {
    let projects: AppProjectsResult = serde_json::from_value(link.call(methods::APP_PROJECTS, json!({})).await?).map_err(|error| RpcError::internal(error.to_string()))?;
    let sessions: AppSessionsResult = serde_json::from_value(link.call(methods::APP_SESSIONS, json!({})).await?).map_err(|error| RpcError::internal(error.to_string()))?;
    let ui_state = match link.call(methods::SETTINGS_GET, json!({ "path": UI_STATE })).await {
        Ok(setting) => setting.get("value").cloned().and_then(|value| serde_json::from_value(value).ok()),
        Err(error) if error.code == codes::NOT_FOUND => None,
        Err(error) => return Err(error),
    };
    let deleted = match link.call(methods::APP_DELETED, json!({})).await {
        Ok(value) => serde_json::from_value::<AppDeletedResult>(value).map_err(|error| RpcError::internal(error.to_string()))?.projects,
        Err(error) if error.code == codes::UNKNOWN_METHOD => Vec::new(),
        Err(error) => return Err(error),
    };
    Ok((projects.projects, sessions.sessions, ui_state, deleted))
}

/// A session's whole journal as the store's stream holds it, in order (empty when it has none).
/// Earlier versions wrote journals there; [`crate::journal::Journal::migrate_streams`] moves them.
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

/// Drop a session's journal stream once its file is written.
pub async fn drop_journal(link: &MemoryLink, session_id: &str) -> Result<(), RpcError> {
    link.call(methods::JOURNAL_DROP, json!({ "session_id": session_id })).await.map(|_| ())
}
