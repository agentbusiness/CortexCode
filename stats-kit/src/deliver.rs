//! Delivery: the rows no CortexManage has accepted yet, offered to each CortexManage in turn.
//!
//! The rows a hand-over just wrote are queued ([`queue`]) and offered at the next round. The day
//! files `<root>/<table>/<part…>/YYYY-MM-DD.parquet` (with an `event_id` column) are read only when
//! a round could not deliver everything, when the queue overflowed, at the first round, or when
//! the caller asks ([`Outbox::rescan`]): then their rows whose event id is not in
//! `YYYY-MM-DD.sent.parquet` beside them are undelivered. Rows go out one table of one day at a
//! time, in pieces, to the first CortexManage that accepts them; the accepted ones are added to the
//! sent list once per round. A CortexManage that answers `accepted: false` is not asked again for
//! [`REFUSED_PAUSE`]. A row is the file's `record_json` when it has one, else its columns. A sent
//! list whose day file is gone is removed.

use crate::day::{read_batches, write_batch};
use arrow_array::{Array, ArrayRef, Int64Array, StringArray};
use arrow_schema::{DataType, Field, Schema};
use cc_protocol::contracts::reports::{SendParams, SendResult, Sender};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// The most rows, and about the most bytes, in one call.
const PIECE_ROWS: usize = 500;
const PIECE_BYTES: usize = 1 << 20;
/// Rows queued between two rounds; past this the queue is dropped and the next round reads the
/// day files instead.
const MAX_QUEUED: usize = 50_000;
/// How long a CortexManage that refused (it does not manage this app's repository) is not asked.
pub const REFUSED_PAUSE: Duration = Duration::from_secs(3600);

static QUEUED: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
static OVERFLOWED: AtomicBool = AtomicBool::new(false);

/// Queue rows just written to their day file for the next round.
pub fn queue(pending: Pending) {
    let Ok(mut queued) = QUEUED.lock() else {
        OVERFLOWED.store(true, Ordering::SeqCst);
        return;
    };
    let held: usize = queued.iter().map(|p| p.rows.len()).sum();
    if held + pending.rows.len() > MAX_QUEUED {
        queued.clear();
        OVERFLOWED.store(true, Ordering::SeqCst);
        return;
    }
    queued.push(pending);
}

fn take_queued() -> Vec<Pending> {
    QUEUED.lock().map(|mut queued| std::mem::take(&mut *queued)).unwrap_or_default()
}

/// Undelivered rows of one day file.
#[derive(Debug, Clone)]
pub struct Pending {
    pub file: PathBuf,
    pub table: String,
    /// The folders between the table and the file (the process, a kind).
    pub part: String,
    pub day: String,
    pub rows: Vec<Value>,
}

/// What a delivery round did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Round {
    pub sent: usize,
    pub waiting: usize,
    /// The node that accepted, when one did.
    pub to: Option<String>,
}

/// The state kept between rounds.
#[derive(Debug)]
pub struct Outbox {
    /// Day files fully sent, by the time they were last changed, so an unchanged one is not read
    /// again.
    done: HashMap<PathBuf, SystemTime>,
    /// Each day file's sent list, read once.
    sent: HashMap<PathBuf, HashSet<String>>,
    /// The tables delivered; empty: every table under the root.
    tables: Vec<String>,
    /// The next round reads the day files.
    backlog: bool,
    /// CortexManages that refused, until when they are not asked.
    paused: HashMap<String, Instant>,
}

impl Default for Outbox {
    fn default() -> Self {
        Outbox { done: HashMap::new(), sent: HashMap::new(), tables: Vec::new(), backlog: true, paused: HashMap::new() }
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// `2026-11-21.parquet` -> `2026-11-21`; None for a sent list, a temporary file or anything else.
fn day_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let day = name.strip_suffix(".parquet")?;
    crate::day::day_number(day).map(|_| day.to_owned())
}

fn sent_path(file: &Path, day: &str) -> PathBuf {
    file.with_file_name(format!("{day}.sent.parquet"))
}

fn day_files(folder: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_dir() {
            day_files(&path, out);
        } else if kind.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && let Some(day) = name.strip_suffix(".sent.parquet")
                && !path.with_file_name(format!("{day}.parquet")).exists()
            {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if day_of(&path).is_some() {
                out.push(path);
            }
        }
    }
}

fn sent_ids(path: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    if !path.exists() {
        return ids;
    }
    for batch in read_batches(path).unwrap_or_default() {
        if let Some(column) = batch.column_by_name("event_id").and_then(|a| a.as_any().downcast_ref::<StringArray>()) {
            ids.extend((0..column.len()).map(|i| column.value(i).to_owned()));
        }
    }
    ids
}

/// A day file's rows, each with its event id.
fn rows(path: &Path) -> Result<Vec<(String, Value)>, String> {
    let mut out = Vec::new();
    for batch in read_batches(path)? {
        let ids = batch.column_by_name("event_id").and_then(|a| a.as_any().downcast_ref::<StringArray>()).ok_or("no event_id column")?.clone();
        let json = batch.column_by_name("record_json").and_then(|a| a.as_any().downcast_ref::<StringArray>()).cloned();
        let objects: Vec<Map<String, Value>> = if json.is_some() {
            Vec::new()
        } else {
            let mut writer = arrow_json::ArrayWriter::new(Vec::new());
            writer.write_batches(&[&batch]).map_err(|e| e.to_string())?;
            writer.finish().map_err(|e| e.to_string())?;
            let bytes = writer.into_inner();
            if bytes.is_empty() { Vec::new() } else { serde_json::from_slice(&bytes).map_err(|e| e.to_string())? }
        };
        for index in 0..ids.len() {
            let row = match &json {
                Some(json) => serde_json::from_str(json.value(index)).map_err(|e| e.to_string())?,
                None => Value::Object(objects.get(index).cloned().unwrap_or_default()),
            };
            out.push((ids.value(index).to_owned(), row));
        }
    }
    Ok(out)
}

impl Outbox {
    /// An outbox for these tables only (the folders right under the root).
    pub fn for_tables(tables: &[&str]) -> Self {
        Outbox { tables: tables.iter().map(|t| (*t).to_owned()).collect(), ..Outbox::default() }
    }

    /// Read the day files at the next round: for a writer that does not [`queue`] its rows.
    pub fn rescan(&mut self) {
        self.backlog = true;
    }

    fn sent_of(&mut self, file: &Path, day: &str) -> &mut HashSet<String> {
        let path = sent_path(file, day);
        self.sent.entry(path.clone()).or_insert_with(|| sent_ids(&path))
    }

    /// Every undelivered row under `root`.
    pub fn pending(&mut self, root: &Path) -> Vec<Pending> {
        let mut files = Vec::new();
        if self.tables.is_empty() {
            day_files(root, &mut files);
        } else {
            for table in &self.tables {
                day_files(&root.join(table), &mut files);
            }
        }
        files.sort();
        let mut out = Vec::new();
        for file in files {
            let changed = modified(&file);
            if changed.is_some() && self.done.get(&file) == changed.as_ref() {
                continue;
            }
            let Some(day) = day_of(&file) else { continue };
            let Ok(relative) = file.strip_prefix(root) else { continue };
            let folders: Vec<String> = relative.parent().map(|p| p.iter().map(|c| c.to_string_lossy().into_owned()).collect()).unwrap_or_default();
            let Some((table, part)) = folders.split_first() else { continue };
            let Ok(all) = rows(&file) else { continue };
            let sent = self.sent_of(&file, &day);
            let rows: Vec<Value> = all.into_iter().filter(|(id, _)| !sent.contains(id)).map(|(_, row)| row).collect();
            if rows.is_empty() {
                if let Some(changed) = changed {
                    self.done.insert(file, changed);
                }
                continue;
            }
            out.push(Pending { file, table: table.clone(), part: part.join("/"), day, rows });
        }
        out
    }

    /// One round: offer the undelivered rows to `targets` (CortexManage nodes, in the order to ask
    /// them), each piece to the first that accepts it. The queued rows are offered; the day files
    /// under `root` are read instead when a backlog is due. A target that fails is not asked again
    /// this round; one that refuses is not asked for [`REFUSED_PAUSE`].
    pub async fn deliver<F, Fut>(&mut self, root: &Path, sender: &Sender, targets: &[String], send: F) -> Round
    where
        F: Fn(String, SendParams) -> Fut,
        Fut: Future<Output = Result<SendResult, String>>,
    {
        let mut round = Round::default();
        let now = Instant::now();
        self.paused.retain(|_, until| *until > now);
        let targets: Vec<String> = targets.iter().filter(|target| !self.paused.contains_key(*target)).cloned().collect();
        let mut queued = take_queued();
        if !self.tables.is_empty() {
            queued.retain(|pending| self.tables.contains(&pending.table));
        }
        if OVERFLOWED.swap(false, Ordering::SeqCst) {
            self.backlog = true;
        }
        if targets.is_empty() {
            // The queued rows are on disk: the next round that has someone to ask reads them.
            if !queued.is_empty() {
                self.backlog = true;
            }
            return round;
        }
        let pending = if std::mem::take(&mut self.backlog) {
            let root = root.to_owned();
            let mut outbox = std::mem::take(self);
            let (outbox, pending) = tokio::task::spawn_blocking(move || {
                let pending = outbox.pending(&root);
                (outbox, pending)
            })
            .await
            .unwrap_or_else(|_| (Outbox::default(), Vec::new()));
            *self = outbox;
            pending
        } else {
            queued
        };
        let mut failed: HashSet<String> = HashSet::new();
        let mut accepted_ids: HashMap<(PathBuf, String), Vec<String>> = HashMap::new();
        for file in pending {
            for piece in pieces(file.rows) {
                let count = piece.len();
                let ids: Vec<String> = piece.iter().filter_map(|row| row.get("event_id").and_then(Value::as_str).map(str::to_owned)).collect();
                let params = SendParams { sender: sender.clone(), table: file.table.clone(), part: file.part.clone(), day: file.day.clone(), rows: piece };
                let mut accepted = None;
                for target in &targets {
                    if failed.contains(target) || self.paused.contains_key(target) {
                        continue;
                    }
                    match send(target.clone(), params.clone()).await {
                        Ok(answer) if answer.accepted => {
                            accepted = Some(target.clone());
                            break;
                        }
                        Ok(_) => {
                            self.paused.insert(target.clone(), Instant::now() + REFUSED_PAUSE);
                        }
                        Err(_) => {
                            failed.insert(target.clone());
                        }
                    }
                }
                match accepted {
                    Some(node) => {
                        accepted_ids.entry((file.file.clone(), file.day.clone())).or_default().extend(ids);
                        round.sent += count;
                        round.to = Some(node);
                    }
                    None => {
                        round.waiting += count;
                        self.backlog = true;
                    }
                }
            }
        }
        for ((file, day), ids) in accepted_ids {
            let path = sent_path(&file, &day);
            self.sent_of(&file, &day).extend(ids.iter().cloned());
            let _ = tokio::task::spawn_blocking(move || mark_sent(&path, &ids)).await;
        }
        round
    }
}

fn pieces(rows: Vec<Value>) -> Vec<Vec<Value>> {
    let mut out: Vec<Vec<Value>> = Vec::new();
    let mut bytes = 0;
    for row in rows {
        let size = row.to_string().len();
        if out.last().is_none_or(|piece| piece.len() >= PIECE_ROWS || (bytes + size > PIECE_BYTES && !piece.is_empty())) {
            out.push(Vec::new());
            bytes = 0;
        }
        bytes += size;
        if let Some(piece) = out.last_mut() {
            piece.push(row);
        }
    }
    out
}

/// Add event ids to a day's sent list.
pub fn mark_sent(path: &Path, ids: &[String]) -> Result<(), String> {
    let mut all: Vec<(String, i64)> = Vec::new();
    if path.exists() {
        for batch in read_batches(path)? {
            let id = batch.column_by_name("event_id").and_then(|a| a.as_any().downcast_ref::<StringArray>()).cloned();
            let at = batch.column_by_name("sent_at_ms").and_then(|a| a.as_any().downcast_ref::<Int64Array>()).cloned();
            if let (Some(id), Some(at)) = (id, at) {
                all.extend((0..id.len()).map(|i| (id.value(i).to_owned(), at.value(i))));
            }
        }
    }
    let now = crate::now_ms();
    let known: HashSet<String> = all.iter().map(|(id, _)| id.clone()).collect();
    all.extend(ids.iter().filter(|id| !known.contains(*id)).map(|id| (id.clone(), now)));
    let schema = Arc::new(Schema::new(vec![Field::new("event_id", DataType::Utf8, false), Field::new("sent_at_ms", DataType::Int64, false)]));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from(all.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>())),
        Arc::new(Int64Array::from(all.iter().map(|(_, at)| *at).collect::<Vec<_>>())),
    ];
    write_batch(path, schema, columns)
}
