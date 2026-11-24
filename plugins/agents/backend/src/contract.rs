//! Host-run contract checks. "Done" means the contract is green as checked by the host, never as
//! claimed by the model. Used by `agents.verify` and by the foreground DO route.

use crate::host::Host;
use cc_protocol::RpcError;
use cc_protocol::contracts::agents::{CheckResult, VerifyReport};
use cc_protocol::contracts::memory::{Contract, ContractCheck};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::tools::{ArtifactPutResult, ArtifactResult, RunResult, methods as tools_methods};
use serde_json::{Value, json};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// A check command may run a full test suite; this only guards against a hung process.
const CHECK_TIMEOUT_MS: u64 = 30 * 60 * 1000;

/// Whether `output` satisfies `expect`: a regular expression, or a plain substring when the text is
/// not a valid expression. Empty expectations always hold.
pub fn expectation_holds(expect: Option<&str>, output: &str) -> bool {
    match expect.map(str::trim).filter(|expect| !expect.is_empty()) {
        None => true,
        Some(expect) => match regex::Regex::new(expect) {
            Ok(pattern) => pattern.is_match(output),
            Err(_) => output.contains(expect),
        },
    }
}

/// Bytes of output a test run may have for its compact view: the tools plugin's own bound
/// (`exec::TEST_VIEW_BYTES` in plugins/tools), so a larger output keeps the view it already has.
const TEST_VIEW_BYTES: u64 = 1 << 20;

/// The id of the artifact where the tools plugin kept a long output whole, read from the marker it
/// puts where it left lines out (`[N lines not shown: read artifact://<id>:<a>-<b> for them]`).
fn kept_whole(view: &str) -> Option<&str> {
    const MARK: &str = " lines not shown: read artifact://";
    let rest = &view[view.find(MARK)? + MARK.len()..];
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or(rest.len());
    Some(&rest[..end]).filter(|id| !id.is_empty())
}

/// The lines of an output that report an error, each whole with the indented lines under it (a
/// compiler's location and code, a traceback's frames, a stack's `at` lines).
pub(crate) fn error_lines(text: &str) -> String {
    const SIGNS: [&str; 8] = ["error", "fail", "panic", "exception", "fatal", "traceback", "cannot", "could not"];
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        let lower = line.to_lowercase();
        let starts = SIGNS.iter().any(|sign| lower.contains(sign));
        let under = inside && !line.trim().is_empty() && line.starts_with(char::is_whitespace);
        inside = starts || under;
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A failing run's output as a report shows it, never cut: a test run's failing tests (the runner's
/// compact report: one line per failing test with its error, the first failure whole) with the
/// pointer to the whole output; any other output whole when the tools plugin returned it whole,
/// else its error lines whole with the pointer to the whole output it kept. A passing run's output
/// is shown as the tools plugin returned it (whole, or its start, the pointer and its end). A log
/// cut to its last characters lost the start of the failures and the pointer to the rest (seen: a
/// verify note that was "[6255 earlier characters not shown]" and a row of dots).
pub async fn shown(host: &Host, view: &str, failing: bool) -> String {
    if !failing {
        return view.to_owned();
    }
    let mut kept = kept_whole(view).map(str::to_owned);
    // The whole output: the artifact the tools plugin kept, or the view itself when it is whole.
    // A kept output larger than a test view may be is left as the tools plugin showed it (its view
    // already points to the whole), so it is never loaded and sent again.
    let whole = match &kept {
        Some(id) => match host.get::<ArtifactResult>(tools_methods::ARTIFACT, json!({"id": id, "range": "1-1"})).await {
            Ok(first) if first.total_bytes <= TEST_VIEW_BYTES => match host.get::<ArtifactResult>(tools_methods::ARTIFACT, json!({"id": id})).await {
                Ok(artifact) if artifact.encoding == "utf8" => artifact.data,
                _ => return view.to_owned(),
            },
            _ => return view.to_owned(),
        },
        None => view.to_owned(),
    };
    let answer = host.call(tools_methods::TESTS_REPORT, json!({"text": whole, "artifact": kept})).await.ok();
    let report = answer
        .as_ref()
        .and_then(|answer| answer.get("report").and_then(Value::as_str).map(str::to_owned))
        .filter(|report| !report.trim().is_empty());
    // The artifact the report kept the output in, when it had to keep it: the pointer names it.
    if kept.is_none() {
        kept = answer.as_ref().and_then(|answer| answer.get("artifact").and_then(Value::as_str)).filter(|id| !id.is_empty()).map(str::to_owned);
    }
    if kept.is_none() && report.is_none() {
        return view.to_owned();
    }
    let lines = whole.lines().count().max(1);
    let id = match kept {
        Some(id) => id,
        None => match host.get::<ArtifactPutResult>(tools_methods::ARTIFACT_PUT, json!({"text": whole})).await {
            Ok(put) => put.id,
            Err(_) => return view.to_owned(),
        },
    };
    if let Some(report) = report {
        return format!("{}\nEvery failure whole: read artifact://{id}:1-{lines}\n", report.trim_end());
    }
    let errors = error_lines(&whole);
    if errors.is_empty() {
        return view.to_owned();
    }
    format!("{errors}[the whole output: read artifact://{id}:1-{lines}]\n")
}

/// Whether a check that did not pass was killed from outside: status 137 (SIGKILL) or 143 (SIGTERM)
/// is the machine stopping the command (out of memory, the machine guard, a stop), never a test
/// that failed. Such a check could not run: it does not judge the work.
pub fn killed(check: &CheckResult) -> bool {
    !check.ok && matches!(check.exit_code, Some(137) | Some(143))
}

/// What a host verification showed. A contract without checks shows nothing: its report's `ok` is
/// true (no check failed), and that is not a pass (seen: nine agent tasks, each verified with
/// `checks: []`, told to the session as "its checks pass in the project", and the whole work then
/// reported done with almost none of it working).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// At least one check ran and every check passed.
    Passed,
    /// A check failed, or the host could not decide one.
    Failed,
    /// No check covers the work: nothing ran.
    NotChecked,
    /// Every check that did not pass was killed by the machine ([`killed`]): the checks could not
    /// run here, which says nothing about the work and is never a failure to fix (seen: a test run
    /// killed with status 143 under memory pressure taken as failing tests).
    Unrun,
}

impl Verdict {
    pub fn of(report: &VerifyReport) -> Verdict {
        if report.checks.is_empty() {
            Verdict::NotChecked
        } else if report.ok {
            Verdict::Passed
        } else if report.checks.iter().filter(|check| !check.ok).all(killed) {
            Verdict::Unrun
        } else {
            Verdict::Failed
        }
    }

    /// The state as records carry it (`state` of a verification entry, of a finished task). Checks
    /// that could not run checked nothing: the same state as no check at all (the report's checks
    /// carry the cause).
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Passed => "passed",
            Verdict::Failed => "failed",
            Verdict::NotChecked | Verdict::Unrun => "not_checked",
        }
    }
}

/// A check's exit status is its verdict, so a pipeline fails when any stage fails (`pytest | tail`
/// would otherwise exit with `tail`'s status, 0, while the tests fail).
pub fn strict(command: &str) -> String {
    format!("set -o pipefail; {command}")
}

/// One executed check with its complete output.
pub struct Ran {
    pub result: CheckResult,
    pub full_output: String,
}

/// The failing tests a failed run names, when its output is a test report that names every one
/// (a report that names fewer, or none, gives None: the failure cannot be compared by names).
fn failed_tests(run: &RunResult) -> Option<Vec<String>> {
    let tests = run.tests.as_ref().filter(|tests| tests.failed > 0)?;
    let names: std::collections::BTreeSet<String> = tests.names.iter().map(|name| name.trim().to_owned()).filter(|name| !name.is_empty()).collect();
    (names.len() >= tests.failed).then(|| names.into_iter().collect())
}

pub async fn run_check(
    host: &Host,
    workspace: &str,
    check: &ContractCheck,
    cancel: &CancellationToken,
) -> Result<Ran, RpcError> {
    let started = Instant::now();
    match check.kind.as_str() {
        "command" => {
            // A check proves the work where it is: run in the root, never somewhere it moved to.
            if let Some(why) = refusal(&check.spec, workspace) {
                let output = format!("[the host did not run this check: {why}]");
                return Ok(Ran {
                    result: CheckResult { name: check.name.clone(), ok: false, exit_code: None, output_tail: output.clone(), duration_ms: 0, indeterminate: false, failed_tests: None },
                    full_output: output,
                });
            }
            // The check runs like the model's own commands (the workspace's write sandbox), with
            // the network: a project's tests may need to fetch what they build with.
            let command = strict(&check.spec);
            let params = json!({"workspace": workspace, "command": command, "timeout_ms": CHECK_TIMEOUT_MS, "background": false});
            let outcome = host.call_with(tools_methods::RUN, params, crate::host::Effect::Idempotent, Some(cancel)).await?;
            let run: RunResult = cc_protocol::parse_params(outcome)?;
            let exit_ok = run.exit_code == Some(0) && !run.timed_out;
            let ok = exit_ok && expectation_holds(check.expect.as_deref(), &run.output);
            // Killed from outside ([`killed`]): the machine, not the work. INDETERMINATE with the
            // cause, so no one is sent to fix code for it.
            if !ok && !run.timed_out && matches!(run.exit_code, Some(137) | Some(143)) {
                let output = format!(
                    "[this check could not run here: the machine killed it (status {}), e.g. for memory; the work is not judged by it]\n{}",
                    run.exit_code.unwrap_or_default(),
                    run.output
                );
                return Ok(Ran {
                    result: CheckResult { name: check.name.clone(), ok: false, exit_code: run.exit_code, output_tail: output.clone(), duration_ms: started.elapsed().as_millis() as u64, indeterminate: true, failed_tests: None },
                    full_output: output,
                });
            }
            let mut notes = String::new();
            if run.timed_out {
                notes.push_str("\n[the check timed out]");
            }
            if exit_ok && !ok {
                notes.push_str(&format!("\n[exit 0, but the output does not contain the expected `{}`]", check.expect.as_deref().unwrap_or("")));
            }
            let output = format!("{}{notes}", run.output);
            let in_report = format!("{}{notes}", shown(host, &run.output, !ok).await);
            Ok(Ran {
                result: CheckResult {
                    name: check.name.clone(),
                    ok,
                    exit_code: run.exit_code,
                    output_tail: in_report,
                    duration_ms: started.elapsed().as_millis() as u64,
                    indeterminate: false,
                    failed_tests: if ok { None } else { failed_tests(&run) },
                },
                full_output: output,
            })
        }
        // A manual (or unknown) check cannot be decided by the host: INDETERMINATE, never passed.
        // The contract is then not green until someone other than the worker judges it.
        kind => {
            let output = format!("[indeterminate: a {kind} check is not machine-verifiable: {}]", check.spec);
            Ok(Ran {
                result: CheckResult { name: check.name.clone(), ok: false, exit_code: None, output_tail: output.clone(), duration_ms: 0, indeterminate: true, failed_tests: None },
                full_output: output,
            })
        }
    }
}

/// Run every check of a contract (sequentially: checks may share build artifacts).
pub async fn verify(
    host: &Host,
    task_id: &str,
    workspace: &str,
    contract: &Contract,
    cancel: &CancellationToken,
) -> Result<(VerifyReport, Vec<String>), RpcError> {
    let mut checks = Vec::new();
    let mut outputs = Vec::new();
    for check in &contract.checks {
        let ran = run_check(host, workspace, check, cancel).await?;
        outputs.push(ran.full_output);
        checks.push(ran.result);
    }
    let ok = checks.iter().all(|check| check.ok);
    let report = VerifyReport { task_id: task_id.to_owned(), ok, checks, contract: contract.clone(), ts_ms: now_ms(), contract_hash: Some(contract.hash()) };
    Ok((report, outputs))
}

/// The failing part of a report, with complete outputs, for the model.
pub fn describe_failures(report: &VerifyReport, outputs: &[String]) -> String {
    let mut text = String::new();
    for (check, output) in report.checks.iter().zip(outputs) {
        if check.ok {
            continue;
        }
        let spec = report.contract.checks.iter().find(|spec| spec.name == check.name).map(|spec| spec.spec.as_str()).unwrap_or("");
        let exit = check.exit_code.map(|code| code.to_string()).unwrap_or_else(|| "none".into());
        text.push_str(&format!("\n$ {spec}\n(exit {exit})\n{}\n", output.trim_end()));
    }
    text
}

pub use cc_protocol::checks::{commands, contract_from, refusal, rooted};
