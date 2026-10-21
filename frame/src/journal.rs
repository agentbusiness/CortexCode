//! Session journals: each session's patches ([`JournalPatch`]) in its stream in the memory plugin's
//! main store (see [`crate::store`]), and an in-memory materialization per session while the frame
//! runs. A session's journal is read from its stream the first time it is needed.
//!
//! Ordering: every mutation assigns its sequence, applies to the materialization, enqueues its
//! patch in the store's outbox and publishes its event while holding one lock, so the stream
//! order, the materialized order and the event order are identical. Callers wait until the store
//! has the patch before they answer, so an acknowledged patch is kept.

use crate::store::{MemoryLink, Op, Outbox};
use cc_protocol::contracts::frame::{JournalEntry, JournalPatch};
use cc_protocol::contracts::now_ms;
use cc_protocol::{RpcError, codes};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

struct SessionLog {
    entries: Vec<JournalEntry>,
    /// Patches applied so far (the journal's revision; see `JournalPatch::rev`).
    rev: u64,
}

pub struct Journal {
    outbox: Outbox,
    link: Arc<MemoryLink>,
    sessions: Mutex<HashMap<String, SessionLog>>,
    /// Sessions deleted while the frame runs: their journals take no more patches.
    deleted: Mutex<std::collections::HashSet<String>>,
}

/// A pending acknowledgement: the store has the patch.
pub struct Durable(oneshot::Receiver<Result<(), String>>);

impl Durable {
    pub async fn wait(self) -> Result<(), RpcError> {
        match self.0.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(RpcError::internal(format!("journal write failed: {message}"))),
            Err(_) => Err(RpcError::new(codes::UNAVAILABLE, "the store writer stopped")),
        }
    }
}

impl Journal {
    pub fn open(outbox: Outbox, link: Arc<MemoryLink>) -> Journal {
        Journal { outbox, link, sessions: Mutex::new(HashMap::new()), deleted: Mutex::new(std::collections::HashSet::new()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionLog>> {
        self.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Append a new entry. `publish` runs under the journal lock with the patch, in order.
    pub fn append(
        &self,
        session_id: &str,
        kind: &str,
        data: Value,
        publish: impl FnOnce(&JournalPatch),
    ) -> Result<(u64, Durable), RpcError> {
        if kind.is_empty() {
            return Err(RpcError::invalid_params("journal entry kind must not be empty"));
        }
        self.ensure_loaded(session_id)?;
        let mut sessions = self.lock();
        let log = self.loaded(&mut sessions, session_id)?;
        let seq = log.entries.len() as u64 + 1;
        let entry = JournalEntry { seq, ts_ms: now_ms(), session_id: session_id.to_owned(), kind: kind.to_owned(), data };
        let patch = JournalPatch::Create { entry, rev: log.rev + 1 };
        apply(log, &patch).map_err(RpcError::invalid_params)?;
        let durable = self.enqueue(session_id, &patch);
        publish(&patch);
        Ok((seq, durable))
    }

    /// `append` text to (when `text` is given) or `set` (when `value` is given) a field of an entry.
    pub fn patch(
        &self,
        session_id: &str,
        seq: u64,
        field: &str,
        text: Option<String>,
        value: Option<Value>,
        publish: impl FnOnce(&JournalPatch),
    ) -> Result<Durable, RpcError> {
        let patch = match (text, value) {
            (Some(text), None) => {
                JournalPatch::Append { session_id: session_id.to_owned(), seq, field: field.to_owned(), text, rev: 0 }
            }
            (None, Some(value)) => {
                JournalPatch::Set { session_id: session_id.to_owned(), seq, field: field.to_owned(), value, rev: 0 }
            }
            _ => return Err(RpcError::invalid_params("exactly one of `text` (append) or `value` (set) is required")),
        };
        let mut patch = patch;
        self.ensure_loaded(session_id)?;
        let mut sessions = self.lock();
        let log = self.loaded(&mut sessions, session_id)?;
        patch.set_rev(log.rev + 1);
        apply(log, &patch).map_err(RpcError::invalid_params)?;
        let durable = self.enqueue(session_id, &patch);
        publish(&patch);
        Ok(durable)
    }

    /// Materialized entries with `seq >= from`, and the revision they reflect.
    pub fn read(&self, session_id: &str, from: u64) -> Result<(Vec<JournalEntry>, u64), RpcError> {
        self.ensure_loaded(session_id)?;
        let mut sessions = self.lock();
        let log = self.loaded(&mut sessions, session_id)?;
        let start = from.max(1) as usize - 1;
        Ok((log.entries.get(start..).map(<[JournalEntry]>::to_vec).unwrap_or_default(), log.rev))
    }

    /// Forget a deleted session's journal: its materialization now; later patches for it are
    /// refused. Its stream goes with its record (the state store's delete, after the patches
    /// already queued).
    pub fn remove(&self, session_id: &str) -> Result<(), RpcError> {
        if !valid_session_id(session_id) {
            return Err(RpcError::invalid_params(format!("invalid session id `{session_id}`")));
        }
        let mut sessions = self.lock();
        self.deleted.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(session_id.to_owned());
        sessions.remove(session_id);
        Ok(())
    }

    /// Wait until everything enqueued so far is in the store.
    pub async fn flush(&self) {
        self.outbox.flush().await;
    }

    /// Flush (the store writer stops with the frame).
    pub async fn close(&self) {
        self.flush().await;
    }

    fn enqueue(&self, session_id: &str, patch: &JournalPatch) -> Durable {
        let (ack, rx) = oneshot::channel();
        self.outbox.send(Op::Patch { session_id: session_id.to_owned(), patch: patch.clone(), ack });
        Durable(rx)
    }

    /// Read a session's journal from its stream the first time it is needed. Callers order their
    /// patches on a connection, so the read happens in place (it takes a moment once per session):
    /// the next patch must find the journal as the store has it.
    fn ensure_loaded(&self, session_id: &str) -> Result<(), RpcError> {
        if !valid_session_id(session_id) {
            return Err(RpcError::invalid_params(format!("invalid session id `{session_id}`")));
        }
        if self.deleted.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(session_id) {
            return Err(RpcError::not_found(format!("session `{session_id}` was deleted")));
        }
        if self.lock().contains_key(session_id) {
            return Ok(());
        }
        let link = self.link.clone();
        let patches = tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(crate::store::read_journal(&link, session_id)))?;
        let log = replay(session_id, patches);
        self.lock().entry(session_id.to_owned()).or_insert(log);
        Ok(())
    }

    fn loaded<'a>(
        &self,
        sessions: &'a mut HashMap<String, SessionLog>,
        session_id: &str,
    ) -> Result<&'a mut SessionLog, RpcError> {
        if self.deleted.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(session_id) {
            return Err(RpcError::not_found(format!("session `{session_id}` was deleted")));
        }
        Ok(sessions.entry(session_id.to_owned()).or_insert(SessionLog { entries: Vec::new(), rev: 0 }))
    }
}

/// Session ids name streams, so they are restricted structurally to a safe character set.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Materialize a journal from its patches. A patch the stream holds twice (a write retried after
/// the store went away mid-call) carries a revision already applied and is skipped; a patch that
/// does not apply is skipped with a warning.
fn replay(session_id: &str, patches: Vec<JournalPatch>) -> SessionLog {
    let mut log = SessionLog { entries: Vec::new(), rev: 0 };
    for (index, patch) in patches.iter().enumerate() {
        if patch.rev() != 0 && patch.rev() <= log.rev {
            continue;
        }
        if let Err(error) = apply(&mut log, patch) {
            tracing::warn!(session_id, record = index, %error, "skipping an inapplicable journal patch");
        }
    }
    log
}

/// Apply a patch; a patch that applies advances the revision by one (lines written before
/// revisions existed carry `rev: 0` and are numbered by their position).
fn apply(log: &mut SessionLog, patch: &JournalPatch) -> Result<(), String> {
    apply_patch(log, patch)?;
    log.rev += 1;
    Ok(())
}

fn apply_patch(log: &mut SessionLog, patch: &JournalPatch) -> Result<(), String> {
    match patch {
        JournalPatch::Create { entry, .. } => {
            let expected = log.entries.len() as u64 + 1;
            if entry.seq != expected {
                return Err(format!("entry seq {} out of order (expected {expected})", entry.seq));
            }
            log.entries.push(entry.clone());
            Ok(())
        }
        JournalPatch::Append { seq, field, text, .. } => {
            let entry = entry_mut(log, *seq)?;
            let slot = field_mut(&mut entry.data, field)?;
            match slot {
                Value::Null => {
                    *slot = Value::String(text.clone());
                    Ok(())
                }
                Value::String(existing) => {
                    existing.push_str(text);
                    Ok(())
                }
                other => Err(format!("field `{field}` is not a string (found {})", type_name(other))),
            }
        }
        JournalPatch::Set { seq, field, value, .. } => {
            let entry = entry_mut(log, *seq)?;
            *field_mut(&mut entry.data, field)? = value.clone();
            Ok(())
        }
    }
}

fn entry_mut(log: &mut SessionLog, seq: u64) -> Result<&mut JournalEntry, String> {
    let count = log.entries.len();
    seq.checked_sub(1)
        .and_then(|index| log.entries.get_mut(index as usize))
        .ok_or_else(|| format!("no entry with seq {seq} (the journal has {count})"))
}

/// Resolve a dot-separated field path inside `data`, creating missing objects (structural rule).
fn field_mut<'a>(data: &'a mut Value, field: &str) -> Result<&'a mut Value, String> {
    if field.is_empty() || field.split('.').any(str::is_empty) {
        return Err(format!("invalid field path `{field}`"));
    }
    let mut current = data;
    for part in field.split('.') {
        if current.is_null() {
            *current = Value::Object(Map::new());
        }
        let found = type_name(current);
        let object = current.as_object_mut().ok_or_else(|| format!("cannot descend into {found} at `{part}` of `{field}`"))?;
        current = object.entry(part.to_owned()).or_insert(Value::Null);
    }
    Ok(current)
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}
