//! The day files the `stats` plugin writes: `<root>/<table>/<part>/YYYY-MM-DD.parquet`, the same
//! columns as the knowledge app's, kept 30 days.

use crate::calls::CallRecord;
use crate::day::{Row, write_days};
use arrow_array::{ArrayRef, Int64Array, StringArray, UInt64Array};
use arrow_schema::{DataType, Field, Schema};
use cc_protocol::contracts::reports::{ErrorRecord, RecordParams, tables};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// Write one batch a process handed over into its table's day files.
pub fn store(root: &Path, batch: RecordParams) -> Result<usize, String> {
    if batch.part.is_empty() || batch.part.contains(['/', '\\']) || batch.part.starts_with('.') {
        return Err(format!("`{}` is not a process's name", batch.part));
    }
    let folder = root.join(&batch.table).join(&batch.part);
    let count = batch.rows.len();
    match batch.table.as_str() {
        tables::ERRORS => write_days(&folder, parse::<ErrorRecord>(batch.rows)?, |r| r.last_at_ms)?,
        tables::CALLS => write_days(&folder, parse::<CallRecord>(batch.rows)?, |r| r.last_at_ms)?,
        other => return Err(format!("no table `{other}`: the tables are {} and {}", tables::ERRORS, tables::CALLS)),
    }
    Ok(count)
}

fn parse<R: DeserializeOwned>(rows: Vec<Value>) -> Result<Vec<R>, String> {
    rows.into_iter().map(|row| serde_json::from_value(row).map_err(|e| e.to_string())).collect()
}

impl Row for ErrorRecord {
    fn event_id(&self) -> &str {
        &self.event_id
    }

    fn schema() -> Schema {
        let text = |name: &str| Field::new(name, DataType::Utf8, false);
        Schema::new(vec![
            text("event_id"),
            text("source"),
            text("kind"),
            text("area"),
            text("target"),
            text("location"),
            text("fingerprint"),
            text("level"),
            text("message"),
            text("version"),
            text("node_id"),
            Field::new("count", DataType::UInt64, false),
            Field::new("first_at_ms", DataType::Int64, false),
            Field::new("last_at_ms", DataType::Int64, false),
            text("record_json"),
        ])
    }

    fn columns(rows: &[Self]) -> Vec<ArrayRef> {
        let text = |get: fn(&ErrorRecord) -> &str| Arc::new(StringArray::from(rows.iter().map(get).collect::<Vec<_>>())) as ArrayRef;
        vec![
            text(|r| r.event_id.as_str()),
            text(|r| r.source.as_str()),
            text(|r| r.kind.as_str()),
            text(|r| r.area.as_str()),
            text(|r| r.target.as_str()),
            text(|r| r.location.as_str()),
            text(|r| r.fingerprint.as_str()),
            text(|r| r.level.as_str()),
            text(|r| r.message.as_str()),
            text(|r| r.version.as_str()),
            text(|r| r.node_id.as_str()),
            Arc::new(UInt64Array::from(rows.iter().map(|r| r.count).collect::<Vec<_>>())),
            Arc::new(Int64Array::from(rows.iter().map(|r| r.first_at_ms).collect::<Vec<_>>())),
            Arc::new(Int64Array::from(rows.iter().map(|r| r.last_at_ms).collect::<Vec<_>>())),
        ]
    }
}

impl Row for CallRecord {
    fn event_id(&self) -> &str {
        &self.event_id
    }

    fn schema() -> Schema {
        let text = |name: &str| Field::new(name, DataType::Utf8, false);
        let count = |name: &str| Field::new(name, DataType::UInt64, false);
        Schema::new(vec![
            text("event_id"),
            text("part"),
            text("method"),
            text("outcome"),
            text("version"),
            text("node_id"),
            count("count"),
            count("total_us"),
            count("max_us"),
            count("prompt_tokens"),
            count("cached_tokens"),
            count("completion_tokens"),
            Field::new("first_at_ms", DataType::Int64, false),
            Field::new("last_at_ms", DataType::Int64, false),
            text("record_json"),
        ])
    }

    fn columns(rows: &[Self]) -> Vec<ArrayRef> {
        let text = |get: fn(&CallRecord) -> &str| Arc::new(StringArray::from(rows.iter().map(get).collect::<Vec<_>>())) as ArrayRef;
        let count = |get: fn(&CallRecord) -> u64| Arc::new(UInt64Array::from(rows.iter().map(get).collect::<Vec<_>>())) as ArrayRef;
        vec![
            text(|r| r.event_id.as_str()),
            text(|r| r.part.as_str()),
            text(|r| r.method.as_str()),
            text(|r| r.outcome.as_str()),
            text(|r| r.version.as_str()),
            text(|r| r.node_id.as_str()),
            count(|r| r.count),
            count(|r| r.total_us),
            count(|r| r.max_us),
            count(|r| r.prompt_tokens),
            count(|r| r.cached_tokens),
            count(|r| r.completion_tokens),
            Arc::new(Int64Array::from(rows.iter().map(|r| r.first_at_ms).collect::<Vec<_>>())),
            Arc::new(Int64Array::from(rows.iter().map(|r| r.last_at_ms).collect::<Vec<_>>())),
        ]
    }
}
