//! What an app records about itself, as the knowledge app does it, and its delivery to a CortexManage.
//!
//! - **errors**: every error the process logs, every panic and every call site that logs far more
//!   than it should, grouped per window by where it happened (`fingerprint`) and counted (the
//!   knowledge app's `error_stats`, the same records). Warnings are never collected.
//! - **calls**: every contract method the process serves, summed per five-minute bucket with its
//!   time, its outcome and, for a model's answer, its tokens.
//! - **models**: the model calls the process makes, summed per five-minute bucket by model, tag,
//!   computer and outcome: their tokens (in, cached, out, reasoning) and their times.
//!
//! Every process collects; once per window ([`SUMMARY_MS`], a panic at once) it hands what it
//! collected to the app's `stats` plugin (`stats.record`), which alone writes the Parquet day
//! files and delivers them
//! (`contracts::reports`). The writing and the delivery are the `files` feature, which only the
//! `stats` plugin (and the knowledge app, which writes its own) builds.
//!
//! A plugin calls [`start`] once, first thing in `main`, in place of its own log setup; the frame
//! calls [`begin`] and hands over [`take`]'s records itself.

pub mod calls;
pub mod errors;
pub mod models;
#[cfg(feature = "files")]
mod day;
#[cfg(feature = "files")]
pub mod deliver;
#[cfg(feature = "files")]
pub mod files;

use cc_protocol::contracts::reports::{RecordParams, SUMMARY_MS, methods, tables};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Days the files keep: today and the previous 29 UTC days.
pub const ARCHIVE_DAYS: i64 = 30;
/// How often the records are handed over (and written): one summary window.
pub const WINDOW: Duration = Duration::from_millis(SUMMARY_MS as u64);
/// The latest a process hands over after a window closes ([`next_window`]): the processes of a
/// computer spread over this, so they do not all write at the same instant.
const SPREAD_MS: i64 = 60_000;
/// When the `stats` plugin delivers after a window closes ([`next_delivery`]): once every process
/// has handed over.
const DELIVER_AFTER_MS: i64 = SPREAD_MS + 30_000;
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
/// The next [`take`] hands over the open buckets too: a panic, the process may be ending.
static URGENT: AtomicBool = AtomicBool::new(false);
/// This process's place within [`SPREAD_MS`], drawn once.
static OFFSET_MS: OnceLock<i64> = OnceLock::new();

pub(crate) fn recorder() -> Option<&'static Recorder> {
    RECORDER.get()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Set up the log (to standard error, filtered by `$<log_env>`, else by `default`) with the error
/// collector beside it, record panics, count served calls, and hand them to the `stats` plugin once
/// per window after the process has joined its frame.
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

/// Record with this recorder: panics, served calls and the model calls made. For a process that
/// sets up its own log with [`errors::ErrorLayer`] in it and hands [`take`]'s records over itself
/// (the frame).
pub fn begin(recorder: Recorder) {
    if RECORDER.set(recorder).is_err() {
        return;
    }
    errors::install_panic_hook();
    cc_protocol::plugin::observe_calls(calls::observe);
    cc_protocol::plugin::observe_outgoing(models::observe);
}

/// What was collected since the last time, as batches for `stats.record`, with the ones not taken
/// before: the errors, and the `calls` and `models` buckets that have closed (after a panic, the
/// open ones too).
pub fn take() -> Vec<RecordParams> {
    let open_too = URGENT.swap(false, Ordering::SeqCst);
    let part = recorder().map(|r| r.part.clone()).unwrap_or_default();
    let mut out: Vec<RecordParams> = HELD.lock().map(|mut held| std::mem::take(&mut *held)).unwrap_or_default();
    let batch = |table: &str, rows: Vec<Value>| RecordParams { table: table.to_owned(), part: part.clone(), rows };
    let errors = errors::drain();
    if !errors.is_empty() {
        out.push(batch(tables::ERRORS, errors));
    }
    let calls = calls::drain(open_too);
    if !calls.is_empty() {
        out.push(batch(tables::CALLS, calls));
    }
    let models = models::drain(open_too);
    if !models.is_empty() {
        out.push(batch(tables::MODELS, models));
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

/// The start of the summary bucket a time falls in.
pub fn bucket_of(ms: i64) -> i64 {
    ms.div_euclid(SUMMARY_MS) * SUMMARY_MS
}

/// How long until `after` milliseconds past the next window boundary.
fn until_past_boundary(after: i64) -> Duration {
    let now = now_ms();
    let mut at = bucket_of(now) + after;
    if at <= now {
        at += SUMMARY_MS;
    }
    Duration::from_millis((at - now) as u64)
}

/// Wait for the next hand-over: just after the next window closes (at this process's place in the
/// spread), or a panic.
pub async fn next_window() {
    let offset = *OFFSET_MS.get_or_init(|| (uuid::Uuid::new_v4().as_u128() % SPREAD_MS as u128) as i64 + 1_000);
    tokio::select! {
        _ = tokio::time::sleep(until_past_boundary(offset)) => {}
        _ = SEND_NOW.notified() => {}
    }
}

/// Wait for the next delivery: once every process has handed over the window that just closed.
pub async fn next_delivery() {
    tokio::time::sleep(until_past_boundary(DELIVER_AFTER_MS)).await;
}

pub(crate) fn send_now() {
    URGENT.store(true, Ordering::SeqCst);
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
