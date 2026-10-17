//! A session's journal as seen by a run: writes go to the frame, and a local mirror of the
//! materialized entries is kept in step so projections do not re-read the whole journal per step.
//!
//! The frame is the source of truth. The mirror holds every entry this run wrote (with the patches
//! it applied) plus every entry other writers appended, fetched incrementally from the lowest seq
//! not yet known.

use crate::host::Host;
use cc_protocol::RpcError;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Entry kinds this runtime writes beyond the frame's documented ones.
pub mod extra_kinds {
    /// `{system, tools, versions, created_ms, repo_map, folded_through?, digested_through?}`: the session's cached prompt
    /// prefix, frozen so every later call (and a hot-swapped successor) renders identical bytes.
    pub const CONTEXT: &str = "context";
}

pub struct Journal {
    host: Host,
    session_id: String,
    entries: BTreeMap<u64, JournalEntry>,
    /// Every seq <= this is in `entries`.
    synced_through: u64,
}

impl Journal {
    pub fn new(host: Host, session_id: &str) -> Self {
        Self { host, session_id: session_id.to_owned(), entries: BTreeMap::new(), synced_through: 0 }
    }

    /// Load the whole journal.
    pub async fn load(host: Host, session_id: &str) -> Result<Self, RpcError> {
        let mut journal = Self::new(host, session_id);
        journal.sync().await?;
        Ok(journal)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    /// Fetch entries other writers appended since the last sync. Returns the new ones.
    pub async fn sync(&mut self) -> Result<Vec<JournalEntry>, RpcError> {
        let fresh = self.host.journal_read(&self.session_id, self.synced_through + 1).await?;
        let mut new = Vec::new();
        for entry in fresh {
            self.synced_through = self.synced_through.max(entry.seq);
            if let std::collections::btree_map::Entry::Vacant(slot) = self.entries.entry(entry.seq) {
                new.push(entry.clone());
                slot.insert(entry);
            }
        }
        Ok(new)
    }

    /// Re-read one entry's materialized state from the frame (after another writer patched it).
    pub async fn refresh_all(&mut self) -> Result<(), RpcError> {
        let fresh = self.host.journal_read(&self.session_id, 1).await?;
        self.entries.clear();
        self.synced_through = 0;
        for entry in fresh {
            self.synced_through = self.synced_through.max(entry.seq);
            self.entries.insert(entry.seq, entry);
        }
        Ok(())
    }

    pub fn entries(&self) -> impl Iterator<Item = &JournalEntry> {
        self.entries.values()
    }

    pub fn entries_vec(&self) -> Vec<JournalEntry> {
        self.entries.values().cloned().collect()
    }

    pub fn get(&self, seq: u64) -> Option<&JournalEntry> {
        self.entries.get(&seq)
    }

    pub fn last_seq(&self) -> u64 {
        self.entries.keys().next_back().copied().unwrap_or(0)
    }

    pub async fn append(&mut self, kind: &str, data: Value) -> Result<u64, RpcError> {
        let seq = self.host.journal_append(&self.session_id, kind, data.clone()).await?;
        self.entries.insert(seq, JournalEntry { seq, ts_ms: now_ms(), session_id: self.session_id.clone(), kind: kind.to_owned(), data });
        if seq == self.synced_through + 1 {
            self.synced_through = seq;
        }
        Ok(seq)
    }

    pub async fn set(&mut self, seq: u64, field: &str, value: Value) -> Result<(), RpcError> {
        self.host.journal_set(&self.session_id, seq, field, value.clone()).await?;
        if let Some(entry) = self.entries.get_mut(&seq) {
            set_field(&mut entry.data, field, value);
        }
        Ok(())
    }

    pub async fn append_text(&mut self, seq: u64, field: &str, text: &str) -> Result<(), RpcError> {
        if text.is_empty() {
            return Ok(());
        }
        self.host.journal_append_text(&self.session_id, seq, field, text).await?;
        if let Some(entry) = self.entries.get_mut(&seq) {
            append_field(&mut entry.data, field, text);
        }
        Ok(())
    }

    /// A host note. With `to_model` it is also part of the model's conversation.
    pub async fn note(&mut self, run_id: &str, purpose: &str, text: &str, to_model: bool) -> Result<u64, RpcError> {
        self.append(kinds::NOTE, json!({"text": text, "purpose": purpose, "run_id": run_id, "host_message": to_model}))
            .await
    }

    /// The newest `status` entry.
    pub fn last_status(&self) -> Option<&JournalEntry> {
        self.entries.values().rev().find(|entry| entry.kind == kinds::STATUS)
    }

    /// Unconsumed steer entries of `kind` (`steer` or `followup`) in journal order.
    pub fn pending_steers(&self, kind: &str) -> Vec<JournalEntry> {
        self.entries
            .values()
            .filter(|entry| {
                entry.kind == kinds::STEER
                    && entry.data.get("kind").and_then(Value::as_str).unwrap_or("steer") == kind
                    && !entry.data.get("consumed").and_then(Value::as_bool).unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// Mark a steer consumed at the current end of the journal (its place in the conversation).
    pub async fn consume_steer(&mut self, seq: u64) -> Result<(), RpcError> {
        let at = self.last_seq();
        self.set(seq, "consumed_at", json!(at)).await?;
        self.set(seq, "consumed", json!(true)).await
    }

    /// The session's frozen prompt prefix, if one was recorded.
    pub fn context_entry(&self) -> Option<&JournalEntry> {
        self.entries.values().rev().find(|entry| entry.kind == extra_kinds::CONTEXT)
    }
}

/// Set a top-level field of an entry's data (mirrors the frame's `set` patch).
pub fn set_field(data: &mut Value, field: &str, value: Value) {
    if !data.is_object() {
        *data = Value::Object(Map::new());
    }
    if let Some(map) = data.as_object_mut() {
        map.insert(field.to_owned(), value);
    }
}

/// Append to a top-level string field (mirrors the frame's `append` patch).
pub fn append_field(data: &mut Value, field: &str, text: &str) {
    if !data.is_object() {
        *data = Value::Object(Map::new());
    }
    if let Some(map) = data.as_object_mut() {
        let current = map.get(field).and_then(Value::as_str).unwrap_or("").to_owned();
        map.insert(field.to_owned(), Value::String(current + text));
    }
}

/// Streams text into one field of one entry, batching patches so a fast model does not issue one
/// frame call per token. Batching is by time only; nothing is dropped.
pub struct TextStream {
    seq: Option<u64>,
    kind: &'static str,
    base: Value,
    pending: String,
    written: String,
    last_flush: Instant,
}

const FLUSH_EVERY: Duration = Duration::from_millis(40);

impl TextStream {
    /// `base` is the entry's data without the text; the entry is created on the first text.
    pub fn new(kind: &'static str, base: Value) -> Self {
        Self { seq: None, kind, base, pending: String::new(), written: String::new(), last_flush: Instant::now() }
    }

    pub fn seq(&self) -> Option<u64> {
        self.seq
    }

    pub fn text(&self) -> String {
        format!("{}{}", self.written, self.pending)
    }

    pub async fn push(&mut self, journal: &mut Journal, text: &str) -> Result<(), RpcError> {
        if text.is_empty() {
            return Ok(());
        }
        self.pending.push_str(text);
        if self.seq.is_none() || self.last_flush.elapsed() >= FLUSH_EVERY {
            self.flush(journal).await?;
        }
        Ok(())
    }

    pub async fn flush(&mut self, journal: &mut Journal) -> Result<(), RpcError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(&mut self.pending);
        match self.seq {
            None => {
                let mut data = self.base.clone();
                set_field(&mut data, "text", Value::String(chunk.clone()));
                set_field(&mut data, "streaming", Value::Bool(true));
                self.seq = Some(journal.append(self.kind, data).await?);
            }
            Some(seq) => journal.append_text(seq, "text", &chunk).await?,
        }
        self.written.push_str(&chunk);
        self.last_flush = Instant::now();
        Ok(())
    }

    /// Flush, then mark the entry finished. When `final_text` differs from what was streamed (the
    /// model plugin's final parse is authoritative) the field is replaced.
    pub async fn finish(&mut self, journal: &mut Journal, final_text: Option<&str>, extra: &[(&str, Value)]) -> Result<(), RpcError> {
        self.flush(journal).await?;
        if let Some(final_text) = final_text
            && final_text != self.written
        {
            match self.seq {
                Some(seq) => journal.set(seq, "text", Value::String(final_text.to_owned())).await?,
                None if !final_text.is_empty() => {
                    let mut data = self.base.clone();
                    set_field(&mut data, "text", Value::String(final_text.to_owned()));
                    set_field(&mut data, "streaming", Value::Bool(true));
                    self.seq = Some(journal.append(self.kind, data).await?);
                }
                None => {}
            }
            self.written = final_text.to_owned();
        }
        if let Some(seq) = self.seq {
            for (field, value) in extra {
                journal.set(seq, field, value.clone()).await?;
            }
            journal.set(seq, "streaming", Value::Bool(false)).await?;
        }
        Ok(())
    }
}
