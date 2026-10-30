//! When a routine runs: the schedule as written (`every 5m`, `every mon,thu at 09:00`, `once at
//! 2026-11-02 09:00`, `in 20m`, `cron 0 9 * * 1`) read into a canonical [`Schedule`], and the next
//! times it fires. Times are local.

use chrono::{DateTime, Datelike, Duration as Span, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

/// The shortest interval: a shorter one is read as this (said in the answer).
pub const MIN_INTERVAL_MS: u64 = 30_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Every `every_ms`, counted from the last firing.
    Interval { every_ms: u64 },
    /// On these weekdays (0 = Monday) at these minutes of the day.
    Calendar { days: Vec<u8>, minutes: Vec<u32> },
    /// Once, at `at_ms`.
    Once { at_ms: u64 },
    /// A five-field cron expression (minute hour day-of-month month day-of-week).
    Cron { expr: String },
}

/// What reading a schedule gave: the schedule and, when the host changed what was written, why.
pub struct Read {
    pub schedule: Schedule,
    pub note: Option<String>,
}

const DAYS: &[(&str, u8)] = &[
    ("monday", 0), ("mon", 0), ("tuesday", 1), ("tues", 1), ("tue", 1), ("wednesday", 2), ("wed", 2), ("thursday", 3), ("thurs", 3), ("thu", 3),
    ("friday", 4), ("fri", 4), ("saturday", 5), ("sat", 5), ("sunday", 6), ("sun", 6),
];

fn day(word: &str) -> Option<u8> {
    DAYS.iter().find(|(name, _)| *name == word).map(|(_, index)| *index)
}

/// `30s`, `5m`, `5 min`, `2 hours`, `3d`, `2w`, `hour` (one), `1.5h`.
pub fn duration_ms(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = if number.is_empty() { 1.0 } else { number.parse().ok()? };
    let unit = unit.trim();
    let per = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
        "d" | "day" | "days" => 86_400_000.0,
        "w" | "wk" | "week" | "weeks" => 604_800_000.0,
        _ => return None,
    };
    let ms = number * per;
    (ms >= 1.0).then_some(ms as u64)
}

/// `9`, `09:00`, `9:30`, `9am`, `9:30pm`, `21:00`, `noon`, `midnight`: the minute of the day.
fn time_of_day(text: &str) -> Option<u32> {
    let text = text.trim();
    match text {
        "noon" => return Some(12 * 60),
        "midnight" => return Some(0),
        _ => {}
    }
    let (body, offset) = if let Some(body) = text.strip_suffix("am") {
        (body.trim(), Some(0))
    } else if let Some(body) = text.strip_suffix("pm") {
        (body.trim(), Some(12))
    } else {
        (text, None)
    };
    let (hour, minute) = match body.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?),
        None => (body.parse::<u32>().ok()?, 0),
    };
    let hour = match offset {
        Some(add) if hour == 12 => add,
        Some(add) if hour < 12 => hour + add,
        Some(_) => return None,
        None => hour,
    };
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

/// Several times: `9:00,17:00` or `9 and 17`.
fn times(text: &str) -> Option<Vec<u32>> {
    let mut out: Vec<u32> = text.split([',', '&']).flat_map(|part| part.split(" and ")).map(time_of_day).collect::<Option<_>>()?;
    out.sort_unstable();
    out.dedup();
    (!out.is_empty()).then_some(out)
}

/// `mon,thu`, `mon-fri`, `weekdays`, `weekends`, `monday and friday`.
fn days(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for part in text.split([',', '&']).flat_map(|part| part.split(" and ")) {
        let part = part.trim().trim_end_matches('s');
        match part {
            "weekday" => out.extend(0..5),
            "weekend" => out.extend(5..7),
            "day" | "everyday" => out.extend(0..7),
            _ => match part.split_once('-') {
                Some((from, to)) => {
                    let (from, to) = (day(from.trim())?, day(to.trim())?);
                    if from <= to { out.extend(from..=to) } else { out.extend((from..7).chain(0..=to)) }
                }
                None => out.push(day(part).or_else(|| day(&format!("{part}s")))?),
            },
        }
    }
    out.sort_unstable();
    out.dedup();
    (!out.is_empty()).then_some(out)
}

fn local_ms(naive: NaiveDateTime) -> Option<u64> {
    Local.from_local_datetime(&naive).earliest().map(|time| time.timestamp_millis().max(0) as u64)
}

fn local(ms: u64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ms as i64).single().unwrap_or_else(Local::now)
}

/// `2026-11-02 09:00`, `2026-11-02T09:00`, `2026-11-02` (09:00), `17:00` (the next one), `tomorrow 9`.
pub fn moment(text: &str, now_ms: u64) -> Option<u64> {
    let text = text.trim().replace('T', " ");
    let now = local(now_ms);
    let (date_part, time_part) = match text.split_once(' ') {
        Some((date, time)) => (Some(date.trim().to_owned()), time.trim().to_owned()),
        None if text.contains('-') => (Some(text.clone()), "09:00".to_owned()),
        None => (None, text.clone()),
    };
    let minute = time_of_day(&time_part)?;
    let time = NaiveTime::from_hms_opt(minute / 60, minute % 60, 0)?;
    match date_part.as_deref() {
        Some("today") => local_ms(now.date_naive().and_time(time)),
        Some("tomorrow") => local_ms((now.date_naive() + Span::days(1)).and_time(time)),
        Some(date) => local_ms(NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?.and_time(time)),
        None => {
            let today = local_ms(now.date_naive().and_time(time))?;
            if today > now_ms { Some(today) } else { local_ms((now.date_naive() + Span::days(1)).and_time(time)) }
        }
    }
}

/// Read a schedule as written.
pub fn read(written: &str, now_ms: u64) -> Result<Read, String> {
    let text = written.trim().to_lowercase().replace(['\u{a0}', '\t'], " ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.as_str();
    if text.is_empty() {
        return Err(HELP.to_owned());
    }
    if let Some(expr) = text.strip_prefix("cron ") {
        cron_fields(expr).map_err(|error| format!("cron `{expr}`: {error}"))?;
        return Ok(Read { schedule: Schedule::Cron { expr: expr.to_owned() }, note: None });
    }
    if let Some(span) = text.strip_prefix("in ") {
        let ms = duration_ms(span).ok_or_else(|| format!("`{span}` is not a duration (5m, 2h, 3d)"))?;
        return Ok(Read { schedule: Schedule::Once { at_ms: now_ms + ms }, note: None });
    }
    if let Some(rest) = text.strip_prefix("once ").or_else(|| text.strip_prefix("at ")).or_else(|| text.strip_prefix("on ")) {
        let rest = rest.trim_start_matches("at ").trim_start_matches("on ");
        let at = moment(rest, now_ms).ok_or_else(|| format!("`{rest}` is not a date and time (2026-11-02 09:00, 17:00, tomorrow 9)"))?;
        return Ok(Read { schedule: Schedule::Once { at_ms: at }, note: None });
    }
    let named = match text {
        "hourly" => Some("every 1h"),
        "daily" => Some("every day at 09:00"),
        "weekly" => Some("every mon at 09:00"),
        _ => None,
    };
    let text = named.unwrap_or(text);
    let body = text.strip_prefix("every ").or_else(|| text.strip_prefix("each ")).unwrap_or(text);
    // `daily at 9`, `weekdays at 8`, `every day at 9`, `every mon,thu at 09:00`, `every week on mon at 9`.
    let (when_days, at) = match body.split_once(" at ") {
        Some((head, at)) => (Some(head.trim()), Some(at.trim())),
        None => (None, None),
    };
    if let (Some(head), Some(at)) = (when_days, at) {
        let head = head.trim_start_matches("week on ").trim_start_matches("week ").trim_start_matches("on ");
        let head = match head {
            "daily" | "day" | "days" => "day",
            other => other,
        };
        let minutes = times(at).ok_or_else(|| format!("`{at}` is not a time of day (09:00, 9am, 17:30)"))?;
        let days = days(head).ok_or_else(|| format!("`{head}` is not a day (mon, mon-fri, weekdays, day)"))?;
        return Ok(Read { schedule: Schedule::Calendar { days, minutes }, note: None });
    }
    if let Some(days) = days(body.trim_start_matches("week on ").trim_start_matches("on ")) {
        // A day without a time: 09:00.
        return Ok(Read { schedule: Schedule::Calendar { days, minutes: vec![9 * 60] }, note: Some("no time given: 09:00".to_owned()) });
    }
    let every = duration_ms(body).ok_or_else(|| format!("cannot read `{written}`. {HELP}"))?;
    if every < MIN_INTERVAL_MS {
        return Ok(Read { schedule: Schedule::Interval { every_ms: MIN_INTERVAL_MS }, note: Some("the shortest interval is 30s".to_owned()) });
    }
    Ok(Read { schedule: Schedule::Interval { every_ms: every }, note: None })
}

pub const HELP: &str = "Write it like: every 5m, every 2h, every day at 09:00, every mon,thu at 09:00, weekdays at 8am, once at 2026-11-02 09:00, in 20m, cron 0 9 * * 1.";

/// The next firing strictly after `after_ms` (None: it never fires again).
pub fn next_after(schedule: &Schedule, after_ms: u64) -> Option<u64> {
    match schedule {
        Schedule::Interval { every_ms } => Some(after_ms + (*every_ms).max(MIN_INTERVAL_MS)),
        Schedule::Once { at_ms } => (*at_ms > after_ms).then_some(*at_ms),
        Schedule::Calendar { days, minutes } => {
            let start = local(after_ms).date_naive();
            for offset in 0..15 {
                let date = start + Span::days(offset);
                if !days.contains(&(date.weekday().num_days_from_monday() as u8)) {
                    continue;
                }
                for minute in minutes {
                    let Some(time) = NaiveTime::from_hms_opt(minute / 60, minute % 60, 0) else { continue };
                    if let Some(ms) = local_ms(date.and_time(time))
                        && ms > after_ms
                    {
                        return Some(ms);
                    }
                }
            }
            None
        }
        Schedule::Cron { expr } => {
            let fields = cron_fields(expr).ok()?;
            cron_next(&fields, after_ms)
        }
    }
}

/// The next `count` firings after `after_ms`.
pub fn upcoming(schedule: &Schedule, after_ms: u64, count: usize) -> Vec<u64> {
    let mut out = Vec::new();
    let mut from = after_ms;
    while out.len() < count {
        let Some(next) = next_after(schedule, from) else { break };
        out.push(next);
        from = next;
    }
    out
}

/// `Mon 2 Nov 09:00`, or `14:05` today.
pub fn show(ms: u64, now_ms: u64) -> String {
    let time = local(ms);
    if time.date_naive() == local(now_ms).date_naive() {
        time.format("today %H:%M").to_string()
    } else if time.year() == local(now_ms).year() {
        time.format("%a %-d %b %H:%M").to_string()
    } else {
        time.format("%a %-d %b %Y %H:%M").to_string()
    }
}

/// `2026-12-01`, `2026-12-01 18:00`: the moment (a bare date: the end of that day).
pub fn until(text: &str, now_ms: u64) -> Option<u64> {
    let text = text.trim();
    if !text.contains(' ') && !text.contains('T') {
        let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
        return local_ms(date.and_hms_opt(23, 59, 59)?);
    }
    moment(text, now_ms)
}

// ---- cron ---------------------------------------------------------------------------------------

struct CronFields {
    minutes: Vec<bool>,
    hours: Vec<bool>,
    dom: Vec<bool>,
    months: Vec<bool>,
    dow: Vec<bool>,
    dom_any: bool,
    dow_any: bool,
}

fn cron_field(text: &str, low: u32, high: u32) -> Result<Vec<bool>, String> {
    let mut set = vec![false; (high + 1) as usize];
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().map_err(|_| format!("bad step `{step}`"))?.max(1)),
            None => (part, 1),
        };
        let (from, to) = match range {
            "*" => (low, high),
            _ => match range.split_once('-') {
                Some((from, to)) => (from.parse().map_err(|_| format!("bad value `{from}`"))?, to.parse().map_err(|_| format!("bad value `{to}`"))?),
                None => {
                    let value: u32 = range.parse().map_err(|_| format!("bad value `{range}`"))?;
                    (value, if part.contains('/') { high } else { value })
                }
            },
        };
        if from < low || to > high || from > to {
            return Err(format!("`{part}` is outside {low}-{high}"));
        }
        let mut value = from;
        while value <= to {
            set[value as usize] = true;
            value += step;
        }
    }
    Ok(set)
}

fn cron_fields(expr: &str) -> Result<CronFields, String> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err("needs five fields: minute hour day-of-month month day-of-week".to_owned());
    }
    let mut dow = cron_field(parts[4], 0, 7)?;
    if dow[7] {
        dow[0] = true;
    }
    Ok(CronFields {
        minutes: cron_field(parts[0], 0, 59)?,
        hours: cron_field(parts[1], 0, 23)?,
        dom: cron_field(parts[2], 1, 31)?,
        months: cron_field(parts[3], 1, 12)?,
        dow,
        dom_any: parts[2] == "*",
        dow_any: parts[4] == "*",
    })
}

fn cron_next(fields: &CronFields, after_ms: u64) -> Option<u64> {
    let start = local(after_ms + 60_000).naive_local();
    let mut date = start.date();
    let mut first_minute = start.hour() * 60 + start.minute();
    for _ in 0..(366 * 5) {
        let day_ok = {
            let dom = fields.dom[date.day() as usize];
            let dow = fields.dow[date.weekday().num_days_from_sunday() as usize];
            match (fields.dom_any, fields.dow_any) {
                (true, true) => true,
                (false, true) => dom,
                (true, false) => dow,
                (false, false) => dom || dow,
            }
        };
        if fields.months[date.month() as usize] && day_ok {
            for minute_of_day in first_minute..(24 * 60) {
                let (hour, minute) = (minute_of_day / 60, minute_of_day % 60);
                if fields.hours[hour as usize]
                    && fields.minutes[minute as usize]
                    && let Some(ms) = NaiveTime::from_hms_opt(hour, minute, 0).and_then(|time| local_ms(date.and_time(time)))
                    && ms > after_ms
                {
                    return Some(ms);
                }
            }
        }
        date = date.succ_opt()?;
        first_minute = 0;
    }
    None
}
