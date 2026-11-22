//! Errors, failures and log floods of this process (the knowledge app's `error_stats`, the same records).
//!
//! Every error the process logs, every panic, and every call site that logs far more than it should
//! is grouped by where it happened (`fingerprint`) and counted. Each group keeps the first message
//! of its window, up to five distinct messages, the fields and the spans it happened in, and the
//! version. Groups are handed over once per window. Warnings are never collected: they stay in the
//! process's own log.

use crate::{OWN_TARGET, now_ms, recorder};
use cc_protocol::contracts::reports::ErrorRecord;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tracing::{Event, Level, Metadata, Subscriber, field::Field as TraceField, span};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

/// Groups held between two flushes; a group past this many is counted as dropped.
const MAX_GROUPS: usize = 4096;
/// Log lines one call site may write in a minute before it is recorded as a flood.
const FLOOD_PER_MINUTE: u64 = 120;
/// Distinct messages kept per group and window.
const MAX_SAMPLES: usize = 5;
/// The longest message, field value or stack kept for one group.
const MAX_TEXT: usize = 16 * 1024;

static GROUPS: OnceLock<Mutex<HashMap<String, ErrorRecord>>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

fn groups() -> &'static Mutex<HashMap<String, ErrorRecord>> {
    GROUPS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One observed failure, before it is grouped ([`record`]).
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub source: &'static str,
    pub kind: String,
    pub area: String,
    pub target: String,
    pub location: String,
    pub level: String,
    pub message: String,
    pub fields: Value,
    pub scenario: Value,
    pub stack: Option<String>,
    pub count: u64,
    pub at_ms: i64,
    /// The version that saw it, when it is not this process's.
    pub version: Option<String>,
}

/// The group a failure belongs to: its source, kind and place, never its message, so the same
/// failure with different values in its text stays one group.
pub fn fingerprint(source: &str, kind: &str, location: &str, target: &str) -> String {
    let digest = Sha256::digest(format!("{source}\u{0}{kind}\u{0}{location}\u{0}{target}").as_bytes());
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

/// Group one failure: the first of its place in this window keeps its message, fields, scenario
/// and stack; the rest add to its count and move its last time. Never waits on anything but the
/// group table, and never logs.
pub fn record(entry: Entry) {
    let fingerprint = fingerprint(entry.source, &entry.kind, &entry.location, &entry.target);
    let message = cut(entry.message);
    let Ok(mut table) = groups().lock() else {
        DROPPED.fetch_add(entry.count.max(1), Ordering::Relaxed);
        return;
    };
    if let Some(group) = table.get_mut(&fingerprint) {
        group.count = group.count.saturating_add(entry.count.max(1));
        group.first_at_ms = group.first_at_ms.min(entry.at_ms);
        group.last_at_ms = group.last_at_ms.max(entry.at_ms);
        if group.samples.len() < MAX_SAMPLES && !group.samples.contains(&message) {
            group.samples.push(message);
        }
        return;
    }
    if table.len() >= MAX_GROUPS {
        DROPPED.fetch_add(entry.count.max(1), Ordering::Relaxed);
        return;
    }
    let (version, node) = recorder().map(|r| (r.version.clone(), r.node.clone())).unwrap_or_default();
    table.insert(
        fingerprint.clone(),
        ErrorRecord {
            schema_version: 1,
            event_id: uuid::Uuid::new_v4().to_string(),
            source: entry.source.to_string(),
            kind: entry.kind,
            area: entry.area,
            target: entry.target,
            location: entry.location,
            fingerprint,
            level: entry.level,
            samples: vec![message.clone()],
            message,
            fields: entry.fields,
            scenario: entry.scenario,
            stack: entry.stack.map(cut),
            count: entry.count.max(1),
            first_at_ms: entry.at_ms,
            last_at_ms: entry.at_ms,
            version: entry.version.unwrap_or(version),
            node_id: node,
        },
    );
}

/// A text cut to `MAX_TEXT` bytes at a character boundary.
fn cut(mut text: String) -> String {
    if text.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// What the calling thread was doing: its name. The spans are added by the layer.
fn scenario() -> Map<String, Value> {
    let mut scenario = Map::new();
    if let Some(name) = std::thread::current().name() {
        scenario.insert("thread".into(), json!(name));
    }
    scenario
}

/// The part of the software a log target belongs to: the module after the crate
/// (`cc_manage::job::…` → `job`), else the target's first segment.
fn area(target: &str) -> String {
    let mut parts = target.split("::");
    let first = parts.next().unwrap_or(target);
    parts.next().map(str::to_string).unwrap_or_else(|| first.to_string())
}

fn location(meta: &Metadata<'_>) -> String {
    match (meta.file(), meta.line()) {
        (Some(file), Some(line)) => format!("{file}:{line}"),
        _ => meta.target().to_string(),
    }
}

/// Records every panic, on any thread or task, with where it happened, its message and a
/// backtrace, then runs the hook that was installed before.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic without a text message".to_string());
        let location = info.location().map(|at| format!("{}:{}", at.file(), at.line())).unwrap_or_default();
        let area = location.split('/').skip_while(|part| *part != "src").nth(1).unwrap_or("panic").trim_end_matches(".rs").to_string();
        record(Entry {
            source: "server",
            kind: "panic".to_string(),
            area,
            target: "panic".to_string(),
            location,
            level: "PANIC".to_string(),
            message,
            fields: Value::Object(Default::default()),
            scenario: Value::Object(scenario()),
            stack: Some(std::backtrace::Backtrace::force_capture().to_string()),
            count: 1,
            at_ms: now_ms(),
            version: None,
        });
        // A panic may end the process: hand what is held over now.
        crate::send_now();
        previous(info);
    }));
}

/// The tracing layer that collects the process's own failures: every error line, with its fields
/// and its spans' fields, and every line counted per call site for floods.
/// Installed beside the log writer, so it sees exactly what the log shows.
pub struct ErrorLayer;

/// The fields a span was opened with, kept on the span for the lines logged inside it.
struct SpanFields(Map<String, Value>);

/// Collects an event's or span's fields as JSON, the message apart.
#[derive(Default)]
struct Fields {
    message: Option<String>,
    fields: Map<String, Value>,
}

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &TraceField, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(text);
        } else {
            self.fields.insert(field.name().to_string(), Value::String(text));
        }
    }
    fn record_str(&mut self, field: &TraceField, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields.insert(field.name().to_string(), Value::String(value.to_string()));
        }
    }
    fn record_i64(&mut self, field: &TraceField, value: i64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
    fn record_u64(&mut self, field: &TraceField, value: u64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
    fn record_bool(&mut self, field: &TraceField, value: bool) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
}

impl<S> Layer<S> for ErrorLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(fields.fields));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        if meta.target().starts_with(OWN_TARGET) {
            return;
        }
        let now = now_ms();
        let collected = *meta.level() == Level::ERROR;
        // The flood count reads only the call site; the fields are read for a line that is
        // collected, or the first of a call site's window.
        let first_of_window = flood::count(meta, now);
        if !collected && !first_of_window {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let message = fields.message.clone().unwrap_or_default();
        if first_of_window {
            flood::sample(meta, &message);
        }
        if !collected {
            return;
        }
        let mut scenario = scenario();
        let spans: Vec<Value> = ctx
            .event_scope(event)
            .map(|scope| {
                scope
                    .from_root()
                    .map(|span| {
                        let fields = span.extensions().get::<SpanFields>().map(|f| Value::Object(f.0.clone())).unwrap_or(Value::Null);
                        json!({ "name": span.name(), "fields": fields })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !spans.is_empty() {
            scenario.insert("spans".into(), Value::Array(spans));
        }
        record(Entry {
            source: "server",
            kind: "error".to_string(),
            area: area(meta.target()),
            target: meta.target().to_string(),
            location: location(meta),
            level: meta.level().to_string(),
            message,
            fields: Value::Object(fields.fields),
            scenario: Value::Object(scenario),
            stack: None,
            count: 1,
            at_ms: now,
            version: None,
        });
    }
}

mod flood {
    use super::*;

    /// Lines of one call site in the current window.
    struct Site {
        meta: &'static Metadata<'static>,
        count: u64,
        first_at_ms: i64,
        last_at_ms: i64,
        sample: String,
    }

    static SITES: OnceLock<Mutex<HashMap<usize, Site>>> = OnceLock::new();

    fn sites() -> &'static Mutex<HashMap<usize, Site>> {
        SITES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Counts one line of a call site; true for the first line of its window, whose message is
    /// then kept as the sample.
    pub(super) fn count(meta: &'static Metadata<'static>, now: i64) -> bool {
        let Ok(mut sites) = sites().try_lock() else {
            return false;
        };
        let key = meta as *const Metadata<'static> as usize;
        match sites.get_mut(&key) {
            Some(site) => {
                site.count += 1;
                site.last_at_ms = now;
                false
            }
            None => {
                sites.insert(key, Site { meta, count: 1, first_at_ms: now, last_at_ms: now, sample: String::new() });
                true
            }
        }
    }

    pub(super) fn sample(meta: &'static Metadata<'static>, message: &str) {
        if let Ok(mut sites) = sites().try_lock()
            && let Some(site) = sites.get_mut(&(meta as *const Metadata<'static> as usize))
        {
            site.sample = message.to_string();
        }
    }

    /// Ends the window: every call site that logged more than `FLOOD_PER_MINUTE` lines a minute
    /// in it, whatever their level, is recorded as a log flood, with its count and its first
    /// message.
    pub(super) fn close_window(window_ms: i64) {
        let taken = match sites().lock() {
            Ok(mut sites) => std::mem::take(&mut *sites),
            Err(_) => return,
        };
        let limit = FLOOD_PER_MINUTE.saturating_mul((window_ms.max(60_000) / 60_000) as u64);
        for site in taken.into_values().filter(|site| site.count > limit) {
            let meta = site.meta;
            let seconds = ((site.last_at_ms - site.first_at_ms).max(1000) / 1000) as u64;
            record(Entry {
                source: "server",
                kind: "log_flood".to_string(),
                area: area(meta.target()),
                target: meta.target().to_string(),
                location: location(meta),
                level: meta.level().to_string(),
                message: format!("{} lines in {seconds} s from one place; first: {}", site.count, site.sample),
                fields: json!({ "lines": site.count, "seconds": seconds, "limit_per_minute": FLOOD_PER_MINUTE }),
                scenario: Value::Object(Default::default()),
                stack: None,
                count: site.count,
                at_ms: site.first_at_ms,
                version: None,
            });
        }
    }
}

/// Ends one window: closes the flood count and takes every group, as rows.
pub(crate) fn drain() -> Vec<Value> {
    flood::close_window(crate::WINDOW.as_millis() as i64);
    let dropped = DROPPED.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        record(Entry {
            source: "server",
            kind: "collector_dropped".to_string(),
            area: "stats".to_string(),
            target: "stats".to_string(),
            location: "stats".to_string(),
            level: "ERROR".to_string(),
            message: format!("{dropped} failures were not grouped: the window held its most groups"),
            fields: json!({ "dropped": dropped }),
            scenario: Value::Object(Default::default()),
            stack: None,
            count: dropped,
            at_ms: now_ms(),
            version: None,
        });
    }
    let taken: Vec<ErrorRecord> = match groups().lock() {
        Ok(mut table) => std::mem::take(&mut *table).into_values().collect(),
        Err(_) => return Vec::new(),
    };
    taken.iter().filter_map(|record| serde_json::to_value(record).ok()).collect()
}
