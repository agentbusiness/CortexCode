//! Session journals: append-only `<home>/journal/<session_id>.jsonl` files of [`JournalPatch`]
//! lines, an in-memory materialization per session, and one writer thread that appends in order
//! and fsyncs in batches.
//!
//! Ordering: every mutation assigns its sequence, applies to the materialization, enqueues its line
//! for the writer and publishes its event while holding one lock, so the file order, the
//! materialized order and the event order are identical. Callers wait for the batch fsync before
//! they answer, so an acknowledged patch is durable.

use cc_protocol::contracts::frame::{JournalEntry, JournalPatch};
use cc_protocol::contracts::now_ms;
use cc_protocol::{RpcError, codes};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::sync::oneshot;

/// Open journal file handles kept between batches.
const MAX_OPEN_JOURNALS: usize = 64;

enum WriterMsg {
    Line { session_id: String, bytes: Vec<u8>, ack: oneshot::Sender<Result<(), String>> },
    Flush(oneshot::Sender<()>),
}

struct SessionLog {
    entries: Vec<JournalEntry>,
    /// Patches applied so far (the journal's revision; see `JournalPatch::rev`).
    rev: u64,
}

pub struct Journal {
    dir: PathBuf,
    sessions: Mutex<HashMap<String, SessionLog>>,
    writer: Mutex<Option<std::sync::mpsc::Sender<WriterMsg>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// A pending durability acknowledgement.
pub struct Durable(oneshot::Receiver<Result<(), String>>);

impl Durable {
    pub async fn wait(self) -> Result<(), RpcError> {
        match self.0.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(RpcError::internal(format!("journal write failed: {message}"))),
            Err(_) => Err(RpcError::new(codes::UNAVAILABLE, "journal writer stopped")),
        }
    }
}

impl Journal {
    pub fn open(dir: PathBuf) -> std::io::Result<Journal> {
        std::fs::create_dir_all(&dir)?;
        let (tx, rx) = std::sync::mpsc::channel();
        let writer_dir = dir.clone();
        let thread = std::thread::Builder::new().name("journal-writer".into()).spawn(move || writer_loop(writer_dir, rx))?;
        Ok(Journal {
            dir,
            sessions: Mutex::new(HashMap::new()),
            writer: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
        })
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
        let mut sessions = self.lock();
        let log = self.loaded(&mut sessions, session_id)?;
        let seq = log.entries.len() as u64 + 1;
        let entry = JournalEntry { seq, ts_ms: now_ms(), session_id: session_id.to_owned(), kind: kind.to_owned(), data };
        let patch = JournalPatch::Create { entry, rev: log.rev + 1 };
        apply(log, &patch).map_err(RpcError::invalid_params)?;
        let durable = self.enqueue(session_id, &patch)?;
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
        let mut sessions = self.lock();
        let log = self.loaded(&mut sessions, session_id)?;
        patch.set_rev(log.rev + 1);
        apply(log, &patch).map_err(RpcError::invalid_params)?;
        let durable = self.enqueue(session_id, &patch)?;
        publish(&patch);
        Ok(durable)
    }

    /// Materialized entries with `seq >= from`, and the revision they reflect.
    pub fn read(&self, session_id: &str, from: u64) -> Result<(Vec<JournalEntry>, u64), RpcError> {
        let mut sessions = self.lock();
        let log = self.loaded(&mut sessions, session_id)?;
        let start = from.max(1) as usize - 1;
        Ok((log.entries.get(start..).map(<[JournalEntry]>::to_vec).unwrap_or_default(), log.rev))
    }

    /// Wait until everything enqueued so far is written and fsynced.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        let sent = self.writer.lock().ok().and_then(|writer| writer.as_ref().map(|w| w.send(WriterMsg::Flush(tx)).is_ok()));
        if sent == Some(true) {
            let _ = rx.await;
        }
    }

    /// Flush and stop the writer thread.
    pub async fn close(&self) {
        self.flush().await;
        if let Ok(mut writer) = self.writer.lock() {
            writer.take();
        }
        let thread = self.thread.lock().ok().and_then(|mut thread| thread.take());
        if let Some(thread) = thread {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }

    fn enqueue(&self, session_id: &str, patch: &JournalPatch) -> Result<Durable, RpcError> {
        let mut bytes = serde_json::to_vec(patch).map_err(|error| RpcError::internal(error.to_string()))?;
        bytes.push(b'\n');
        let (ack, rx) = oneshot::channel();
        let writer = self.writer.lock().map_err(|_| RpcError::internal("journal writer lock poisoned"))?;
        let sender = writer.as_ref().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "the frame is shutting down"))?;
        sender
            .send(WriterMsg::Line { session_id: session_id.to_owned(), bytes, ack })
            .map_err(|_| RpcError::new(codes::UNAVAILABLE, "journal writer stopped"))?;
        Ok(Durable(rx))
    }

    fn loaded<'a>(
        &self,
        sessions: &'a mut HashMap<String, SessionLog>,
        session_id: &str,
    ) -> Result<&'a mut SessionLog, RpcError> {
        if !valid_session_id(session_id) {
            return Err(RpcError::invalid_params(format!("invalid session id `{session_id}`")));
        }
        if !sessions.contains_key(session_id) {
            let log = load(&self.dir, session_id).map_err(|error| RpcError::internal(error.to_string()))?;
            sessions.insert(session_id.to_owned(), log);
        }
        sessions.get_mut(session_id).ok_or_else(|| RpcError::internal("session log vanished"))
    }
}

/// Session ids name files, so they are restricted structurally to a safe character set.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn journal_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.jsonl"))
}

/// Replay a journal file. A torn final line (crash mid-write) is cut off so later appends stay
/// line-aligned; a corrupt line in the middle is skipped with a warning.
fn load(dir: &Path, session_id: &str) -> std::io::Result<SessionLog> {
    let path = journal_path(dir, session_id);
    let mut log = SessionLog { entries: Vec::new(), rev: 0 };
    let mut file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(log),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let complete = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(last) => last + 1,
        None => 0,
    };
    if complete < bytes.len() {
        tracing::warn!(session_id, torn_bytes = bytes.len() - complete, "cutting a torn final journal line");
        file.set_len(complete as u64)?;
        file.seek(SeekFrom::End(0))?;
        file.sync_all()?;
    }
    for (index, line) in bytes[..complete].split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<JournalPatch>(line) {
            Ok(patch) => {
                if let Err(error) = apply(&mut log, &patch) {
                    tracing::warn!(session_id, line = index + 1, %error, "skipping an inapplicable journal patch");
                }
            }
            Err(error) => tracing::warn!(session_id, line = index + 1, %error, "skipping a corrupt journal line"),
        }
    }
    Ok(log)
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

/// Batches: block for the first message, take everything queued behind it, write all lines in
/// order, fsync each touched file once, then acknowledge.
fn writer_loop(dir: PathBuf, rx: std::sync::mpsc::Receiver<WriterMsg>) {
    let mut files: HashMap<String, File> = HashMap::new();
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(next) = rx.try_recv() {
            batch.push(next);
        }
        let mut acks: Vec<(String, oneshot::Sender<Result<(), String>>)> = Vec::new();
        let mut flushes = Vec::new();
        let mut failed: HashMap<String, String> = HashMap::new();
        let mut dirty: Vec<String> = Vec::new();
        for message in batch {
            match message {
                WriterMsg::Line { session_id, bytes, ack } => {
                    if !failed.contains_key(&session_id) {
                        if let Err(error) = write_line(&dir, &mut files, &session_id, &bytes) {
                            failed.insert(session_id.clone(), error.to_string());
                            files.remove(&session_id);
                        } else if !dirty.contains(&session_id) {
                            dirty.push(session_id.clone());
                        }
                    }
                    acks.push((session_id, ack));
                }
                WriterMsg::Flush(done) => flushes.push(done),
            }
        }
        for session_id in dirty {
            if failed.contains_key(&session_id) {
                continue;
            }
            if let Some(file) = files.get(&session_id)
                && let Err(error) = file.sync_data()
            {
                failed.insert(session_id.clone(), error.to_string());
                files.remove(&session_id);
            }
        }
        for (session_id, ack) in acks {
            let outcome = match failed.get(&session_id) {
                Some(error) => {
                    tracing::error!(session_id, %error, "journal write failed");
                    Err(error.clone())
                }
                None => Ok(()),
            };
            let _ = ack.send(outcome);
        }
        for done in flushes {
            let _ = done.send(());
        }
        // Everything is synced here; bound the number of open journal files (one per session
        // written since the last trim) so many sessions cannot exhaust file descriptors.
        if files.len() > MAX_OPEN_JOURNALS {
            files.clear();
        }
    }
    for (_, file) in files {
        let _ = file.sync_all();
    }
}

fn write_line(dir: &Path, files: &mut HashMap<String, File>, session_id: &str, bytes: &[u8]) -> std::io::Result<()> {
    if !files.contains_key(session_id) {
        let file = OpenOptions::new().create(true).append(true).open(journal_path(dir, session_id))?;
        files.insert(session_id.to_owned(), file);
    }
    match files.get_mut(session_id) {
        Some(file) => file.write_all(bytes),
        None => Err(std::io::Error::other("journal file handle missing")),
    }
}
