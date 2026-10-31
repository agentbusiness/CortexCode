//! Command routines on the clock of the CortexTerm on this Mac (its shared service
//! `system.schedule`, docs/STANDARD-CHANGES.md "delegated commands and schedules"). A command
//! routine whose time CortexTerm can say (`daily HH:MM`, `hourly`, `every N min` with N at least 5,
//! `weekly <day> HH:MM`) becomes a CortexTerm schedule owned by this app: CortexTerm runs it, also
//! while CortexCode is closed, through its one runner (risk check, sandbox, trace), and tells this
//! app each run (`system.schedule.done`). The routine keeps the schedule's id; its runs are recorded
//! here from those events. Any other time, or no CortexTerm, keeps this plugin's clock.

use crate::when::Schedule;
use cc_protocol::Ctx;
use cc_protocol::contracts::memory::Routine;
use cc_service_kit::client::{self, Asker, Provider};
use serde_json::{Value, json};
use std::time::Duration;

pub const SERVICE: &str = "system.schedule";
const VERSION: u32 = 1;
const ASK: Duration = Duration::from_secs(10);
const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// The routine's time as CortexTerm writes it, when it can.
pub fn when_of(schedule: &Schedule) -> Option<String> {
    match schedule {
        Schedule::Interval { every_ms } if every_ms % 60_000 == 0 => match every_ms / 60_000 {
            60 => Some("hourly".into()),
            minutes if minutes >= 5 => Some(format!("every {minutes} min")),
            _ => None,
        },
        Schedule::Calendar { days, minutes } if minutes.len() == 1 => {
            let at = format!("{:02}:{:02}", minutes[0] / 60, minutes[0] % 60);
            match days.len() {
                7 => Some(format!("daily {at}")),
                1 => DAYS.get(days[0] as usize).map(|day| format!("weekly {day} {at}")),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The CortexTerm on this Mac that keeps schedules, if there is one now.
pub async fn provider(ctx: &Ctx) -> Option<Provider> {
    client::provider(ctx, SERVICE, VERSION).await
}

fn asker(routine: &Routine) -> Asker {
    let session = routine.report_to.clone().or_else(|| routine.created_session.clone()).unwrap_or_default();
    Asker { session_id: session, by: if routine.by == "user" { "person".into() } else { "agent".into() } }
}

fn fields(routine: &Routine, when: &str, cwd: &str, enabled: bool) -> Value {
    json!({
        "name": routine.title,
        "kind": "command",
        "command": routine.command,
        "cwd": cwd,
        "when": when,
        "enabled": enabled,
        "sandboxed": true,
        "writable": [cwd],
        "purpose": "check",
    })
}

/// Create the routine's schedule there: its id.
pub async fn create(ctx: &Ctx, provider: &Provider, routine: &Routine, when: &str, cwd: &str) -> Result<String, String> {
    let answer = client::change(ctx, provider, "system.schedule.create", fields(routine, when, cwd, routine.state == "active"), &asker(routine), ASK).await.map_err(|error| error.message)?;
    answer.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_owned).ok_or_else(|| "CortexTerm answered without the schedule's id".to_owned())
}

/// Send the routine's schedule there again, whole (its time, command, on or off).
pub async fn edit(ctx: &Ctx, provider: &Provider, routine: &Routine, when: &str, cwd: &str) -> Result<(), String> {
    let mut params = fields(routine, when, cwd, routine.state == "active");
    params["id"] = json!(routine.provider_schedule);
    client::change(ctx, provider, "system.schedule.edit", params, &asker(routine), ASK).await.map(|_| ()).map_err(|error| error.message)
}

pub async fn delete(ctx: &Ctx, provider: &Provider, id: &str, routine: Option<&Routine>) -> Result<(), String> {
    let who = routine.map(asker).unwrap_or_default();
    client::change(ctx, provider, "system.schedule.delete", json!({"id": id}), &who, ASK).await.map(|_| ()).map_err(|error| error.message)
}

/// Run it there now; the run comes back as `system.schedule.done`.
pub async fn run_now(ctx: &Ctx, provider: &Provider, routine: &Routine) -> Result<(), String> {
    let (items, _ignored) = tokio::sync::mpsc::unbounded_channel();
    match client::run(ctx, provider, "system.schedule.run", json!({"id": routine.provider_schedule}), &asker(routine), items, tokio_util::sync::CancellationToken::new()).await {
        client::Ran::Job(_) => Ok(()),
        client::Ran::Elsewhere(error) | client::Ran::Failed(error) => Err(error.message),
    }
}

/// The schedule's finished runs kept there (a week), newest first, as jobs. `runs.list` answers
/// the runs' traces (`{traces: [{id, started, ended, exit_code, command, …}]}`); a run's id is its
/// trace's, as in `system.schedule.done`.
pub async fn runs(ctx: &Ctx, provider: &Provider, routine: &Routine) -> Vec<cc_protocol::contracts::service::Job> {
    let answer = client::read(ctx, provider, "system.schedule.runs.list", json!({"id": routine.provider_schedule, "limit": 50}), &asker(routine), ASK).await;
    let Ok(answer) = answer else { return Vec::new() };
    answer.get("traces").and_then(Value::as_array).into_iter().flatten().filter_map(job_of_trace).collect()
}

/// A run's trace as a job; None while it still runs.
fn job_of_trace(trace: &Value) -> Option<cc_protocol::contracts::service::Job> {
    let id = trace.get("id").and_then(Value::as_str)?.to_owned();
    let started = trace.get("started").and_then(Value::as_u64).unwrap_or(0);
    let ended = trace.get("ended").and_then(Value::as_u64)?;
    let exit_code = trace.get("exit_code").and_then(Value::as_i64).map(|code| code as i32);
    let duration_ms = ended.saturating_sub(started);
    let state = if exit_code == Some(0) { "done" } else { "failed" };
    let code = exit_code.map(|code| format!("exit {code}")).unwrap_or_else(|| "no exit code".to_owned());
    Some(cc_protocol::contracts::service::Job {
        text: format!("{state}: {code} in {}. Its output: read trace://{id}", cc_service_kit::text::duration(duration_ms)),
        trace: Some(format!("trace://{id}")),
        id,
        state: state.into(),
        started,
        ended: Some(ended),
        exit_code,
        duration_ms,
        ..Default::default()
    })
}
