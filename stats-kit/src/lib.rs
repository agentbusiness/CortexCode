//! What an app records about itself, as the knowledge app does it, and its delivery to a CortexManage.
//!
//! - **errors**: every warning and error the process logs, every panic and every call site that
//!   logs far more than it should, grouped per minute by where it happened (`fingerprint`) and
//!   counted (the knowledge app's `error_stats`, the same records).
//! - **calls**: every contract method the process serves, counted per minute with its time, its
//!   outcome and, for a model's answer, its tokens.
//!
//! Every process collects; once a minute it hands what it collected to the app's `stats` plugin
//! (`stats.record`), which alone writes the Parquet day files and delivers them
//! (`contracts::reports`). The writing and the delivery are the `files` feature, which only the
//! `stats` plugin (and the knowledge app, which writes its own) builds.
//!
//! A plugin calls [`start`] once, first thing in `main`, in place of its own log setup; the frame
//! calls [`begin`] and hands over [`take`]'s records itself.

pub mod calls;
pub mod errors;
#[cfg(feature = "files")]
mod day;
#[cfg(feature = "files")]
pub mod deliver;
#[cfg(feature = "files")]
pub mod files;

use cc_protocol::contracts::reports::{RecordParams, methods, tables};
use serde_json::Value;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Days the files keep: today and the previous 29 UTC days.
pub const ARCHIVE_DAYS: i64 = 30;
/// How often the records are handed over (and written).
pub const WINDOW: Duration = Duration::from_secs(60);
/// The module the kit's own lines come from: never collected, so a failing write cannot feed
/// itself.
const OWN_TARGET: &str = "cc_stats_kit";
/// Batches held while the `stats` plugin does not take them; older ones go first.
const MAX_HELD: usize = 64;

/// Who records: the process (the folder its rows go to) and the version stamped on them.
#[derive(Debug, Clone)]
pub struct Recorder {
    /// This process (`cc-manage`, `frame`).
    pub part: String,
    pub version: String,
    /// The node the records are from; empty when the process does not know it.
    pub node: String,
}

static RECORDER: OnceLock<Recorder> = OnceLock::new();
static HELD: Mutex<Vec<RecordParams>> = Mutex::new(Vec::new());
/// A panic was recorded: hand it over now, the process may be ending.
static SEND_NOW: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub(crate) fn recorder() -> Option<&'static Recorder> {
    RECORDER.get()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Set up the log (to standard error, filtered by `$<log_env>`, else by `default`) with the error
/// collector beside it, record panics, count served calls, and hand them to the `stats` plugin once
/// a minute after the process has joined its frame.
pub fn start(log_env: &str, default: &str, part: &str, version: &str) {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_env(log_env).unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let log = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    // A process that set up its log already keeps it: its lines are then not collected, its panics
    // and calls are.
    let _ = tracing_subscriber::registry().with(filter).with(log).with(errors::ErrorLayer).try_init();
    begin(Recorder { part: part.to_owned(), version: version.to_owned(), node: String::new() });
    cc_protocol::plugin::on_connected(hand_over);
}

/// Record with this recorder: panics and served calls. For a process that sets up its own log
/// with [`errors::ErrorLayer`] in it and hands [`take`]'s records over itself (the frame).
pub fn begin(recorder: Recorder) {
    if RECORDER.set(recorder).is_err() {
        return;
    }
    errors::install_panic_hook();
    cc_protocol::plugin::observe_calls(calls::observe);
}

/// What was collected since the last time, as batches for `stats.record`, with the ones not taken
/// before.
pub fn take() -> Vec<RecordParams> {
    let part = recorder().map(|r| r.part.clone()).unwrap_or_default();
    let mut out: Vec<RecordParams> = HELD.lock().map(|mut held| std::mem::take(&mut *held)).unwrap_or_default();
    let batch = |table: &str, rows: Vec<Value>| RecordParams { table: table.to_owned(), part: part.clone(), rows };
    let errors = errors::drain();
    if !errors.is_empty() {
        out.push(batch(tables::ERRORS, errors));
    }
    let calls = calls::drain();
    if !calls.is_empty() {
        out.push(batch(tables::CALLS, calls));
    }
    out
}

/// Batches the `stats` plugin did not take: kept for the next time.
pub fn give_back(batches: Vec<RecordParams>) {
    if let Ok(mut held) = HELD.lock() {
        held.extend(batches);
        let over = held.len().saturating_sub(MAX_HELD);
        held.drain(..over);
    }
}

/// Wait for the next hand-over: a minute, or a panic.
pub async fn next_window() {
    tokio::select! {
        _ = tokio::time::sleep(WINDOW) => {}
        _ = SEND_NOW.notified() => {}
    }
}

pub(crate) fn send_now() {
    SEND_NOW.notify_one();
}

/// A plugin joined its frame: hand the records to `stats` for as long as it runs.
fn hand_over(ctx: cc_protocol::Ctx) {
    tokio::spawn(async move {
        loop {
            next_window().await;
            let mut batches = take().into_iter();
            while let Some(batch) = batches.next() {
                let Ok(params) = serde_json::to_value(&batch) else { continue };
                if ctx.call(methods::RECORD, params).await.is_err() {
                    let mut back = vec![batch];
                    back.extend(batches);
                    give_back(back);
                    break;
                }
            }
        }
    });
}
