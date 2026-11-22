//! Day files: `YYYY-MM-DD.parquet` with an `event_id` column, rewritten whole on each write, and
//! the 30 days they are kept.

use crate::{ARCHIVE_DAYS, now_ms};
use arrow_array::{Array, ArrayRef, RecordBatch, StringArray};
use arrow_schema::Schema;
use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::{fs, path::Path, sync::Arc};

pub(crate) const DAY_MS: i64 = 86_400_000;

/// `YYYY-MM-DD` of a time in milliseconds (UTC).
pub fn day_name(ms: i64) -> String {
    let (year, month, day) = civil(ms.div_euclid(DAY_MS));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 of a `YYYY-MM-DD` name; None for anything else.
pub(crate) fn day_number(name: &str) -> Option<i64> {
    let bytes = name.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = name.get(0..4)?.parse().ok()?;
    let month: i64 = name.get(5..7)?.parse().ok()?;
    let day: i64 = name.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, month, day)
}

/// A table's rows: their event id, their columns, and the whole record as `record_json`.
pub(crate) trait Row: Serialize + DeserializeOwned {
    fn event_id(&self) -> &str;
    fn schema() -> Schema;
    /// The columns before `record_json`, in the order of [`Row::schema`].
    fn columns(rows: &[Self]) -> Vec<ArrayRef>;
}

/// Add rows to a day file: the file's rows are read back by their `record_json`, the new ones
/// appended (a replayed event id replaces its row), and the whole day written to a temporary
/// file, synced and moved over the old one.
pub(crate) fn write_day<R: Row>(path: &Path, records: Vec<R>) -> Result<(), String> {
    let replaced: std::collections::HashSet<String> = records.iter().map(|record| record.event_id().to_owned()).collect();
    let mut rows: Vec<R> = read_records(path)?;
    rows.retain(|row| !replaced.contains(row.event_id()));
    rows.extend(records);
    let json = rows.iter().map(serde_json::to_string).collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    let mut columns = R::columns(&rows);
    columns.push(Arc::new(StringArray::from(json)));
    write_batch(path, Arc::new(R::schema()), columns)
}

pub(crate) fn write_batch(path: &Path, schema: Arc<Schema>, columns: Vec<ArrayRef>) -> Result<(), String> {
    let batch = RecordBatch::try_new(schema.clone(), columns).map_err(|e| e.to_string())?;
    let temp = path.with_extension("parquet.tmp");
    let mut file = fs::File::create(&temp).map_err(|e| e.to_string())?;
    let mut writer = ArrowWriter::try_new(&mut file, schema, None).map_err(|e| e.to_string())?;
    writer.write(&batch).map_err(|e| e.to_string())?;
    writer.close().map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&temp, path).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Every record of a day file, from its `record_json`; none when there is no file.
pub(crate) fn read_records<R: DeserializeOwned>(path: &Path) -> Result<Vec<R>, String> {
    let mut rows = Vec::new();
    if !path.exists() {
        return Ok(rows);
    }
    for batch in read_batches(path)? {
        let column = batch
            .column_by_name("record_json")
            .and_then(|a| a.as_any().downcast_ref::<StringArray>())
            .ok_or("the day file has no record_json column")?;
        for index in 0..column.len() {
            rows.push(serde_json::from_str(column.value(index)).map_err(|e| e.to_string())?);
        }
    }
    Ok(rows)
}

pub(crate) fn read_batches(path: &Path) -> Result<Vec<RecordBatch>, String> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;
    reader.map(|batch| batch.map_err(|e| e.to_string())).collect()
}

/// Write records into the day file of each one's time, then remove what is past its days. Returns
/// what was written: each day file with its day and its new rows.
pub(crate) fn write_days<R: Row>(folder: &Path, records: Vec<R>, at: fn(&R) -> i64) -> Result<Vec<(std::path::PathBuf, String, Vec<serde_json::Value>)>, String> {
    let mut written = Vec::new();
    if records.is_empty() {
        return Ok(written);
    }
    let mut by_day: std::collections::BTreeMap<String, Vec<R>> = Default::default();
    for record in records {
        by_day.entry(day_name(at(&record))).or_default().push(record);
    }
    fs::create_dir_all(folder).map_err(|error| format!("{} could not be created: {error}", folder.display()))?;
    for (name, records) in by_day {
        let rows: Vec<serde_json::Value> = records.iter().filter_map(|record| serde_json::to_value(record).ok()).collect();
        let path = folder.join(format!("{name}.parquet"));
        write_day(&path, records).map_err(|error| format!("the statistics of {name} in {} could not be written: {error}", folder.display()))?;
        written.push((path, name, rows));
    }
    cleanup(folder)?;
    Ok(written)
}

/// Remove day files older than the days kept, their sent lists and temporary files included;
/// nothing else in the folder is touched and no link is followed.
pub(crate) fn cleanup(folder: &Path) -> Result<(), String> {
    if !folder.exists() {
        return Ok(());
    }
    let today = now_ms().div_euclid(DAY_MS);
    for file in fs::read_dir(folder).map_err(|e| e.to_string())? {
        let file = file.map_err(|e| e.to_string())?;
        if !file.file_type().map_err(|e| e.to_string())?.is_file() {
            continue;
        }
        let path = file.path();
        let Some(name) = path.file_name().and_then(|v| v.to_str()) else { continue };
        let Some(day) = name.get(0..10).and_then(day_number) else { continue };
        let rest = &name[10..];
        if !matches!(rest, ".parquet" | ".parquet.tmp" | ".sent.parquet" | ".sent.parquet.tmp") {
            continue;
        }
        if day <= today - ARCHIVE_DAYS {
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
