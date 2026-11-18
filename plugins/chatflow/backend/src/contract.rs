//! Host-run contract checks. "Done" means the contract is green as checked by the host, never as
//! claimed by the model. Used by `agents.verify` and by the foreground DO route.

use crate::host::Host;
use cc_protocol::RpcError;
use cc_protocol::contracts::agents::{CheckResult, VerifyReport};
use cc_protocol::contracts::memory::{Contract, ContractCheck};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::tools::{RunResult, methods as tools_methods};
use serde_json::json;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// A check command may run a full test suite; this only guards against a hung process.
const CHECK_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// Characters of output kept in a report's `output_tail`. As large as what the tools plugin
/// returns for a long output (its own cut: the start, a pointer to the saved whole, the end), so a
/// failing check is not cut a second time here: the last 6000 characters alone lost the start of
/// the failures and the pointer to the rest (seen: a verify note that was "[6255 earlier
/// characters not shown]" and a row of dots).
const TAIL_CHARS: usize = 16 * 1024;

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

fn tail(text: &str) -> String {
    let count = text.chars().count();
    if count <= TAIL_CHARS {
        return text.to_owned();
    }
    let skipped = count - TAIL_CHARS;
    let kept: String = text.chars().skip(skipped).collect();
    format!("[{skipped} earlier characters not shown]\n{kept}")
}

/// Whether a check that did not pass was killed from outside: status 137 (SIGKILL) or 143 (SIGTERM)
/// is the machine stopping the command (out of memory, the machine guard, a stop), never a test
/// that failed. Such a check could not run: it does not judge the work.
pub fn killed(check: &CheckResult) -> bool {
    !check.ok && matches!(check.exit_code, Some(137) | Some(143))
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
            // The check's own tool is not installed here (`python -m pytest` without pytest, a
            // command not found): the check is broken in itself, not a failure of the work to fix.
            // INDETERMINATE, with the cause named (seen: a model sent to "fix" a missing pytest
            // for ten minutes, then the turn ended as a repeated contract failure).
            if !ok && missing_tool(&command, run.exit_code, &run.output) {
                let output = format!("[this check cannot run here: {}; the work is not judged by it]", missing_tool_cause(&command, &run.output));
                return Ok(Ran {
                    result: CheckResult { name: check.name.clone(), ok: false, exit_code: run.exit_code, output_tail: output.clone(), duration_ms: started.elapsed().as_millis() as u64, indeterminate: true, failed_tests: None },
                    full_output: output,
                });
            }
            // The sandbox stopped the check itself ("Operation not permitted": it writes where commands
            // may not, e.g. a build cache outside the project): the environment, not the work, so it
            // cannot judge the work. INDETERMINATE with the line that says so (seen: a model trying
            // to change the user's Go and shell settings for half an hour to turn such a check green).
            if !ok && let Some(line) = sandbox_refused(&run.output) {
                let output = format!("[this check cannot run here: the sandbox stopped it ({line}); the work is not judged by it]");
                return Ok(Ran {
                    result: CheckResult { name: check.name.clone(), ok: false, exit_code: run.exit_code, output_tail: output.clone(), duration_ms: started.elapsed().as_millis() as u64, indeterminate: true, failed_tests: None },
                    full_output: output,
                });
            }
            // Killed from outside ([`killed`]): the machine, not the work. INDETERMINATE with the
            // cause, so no one is sent to fix code for it (seen: `cargo test` killed with status 143
            // under memory pressure, taken as failing tests).
            if !ok && !run.timed_out && matches!(run.exit_code, Some(137) | Some(143)) {
                let output = format!(
                    "[this check could not run here: the machine killed it (status {}), e.g. for memory; the work is not judged by it]\n{}",
                    run.exit_code.unwrap_or_default(),
                    tail(&run.output)
                );
                return Ok(Ran {
                    result: CheckResult { name: check.name.clone(), ok: false, exit_code: run.exit_code, output_tail: output.clone(), duration_ms: started.elapsed().as_millis() as u64, indeterminate: true, failed_tests: None },
                    full_output: output,
                });
            }
            let mut output = run.output.clone();
            if run.timed_out {
                output.push_str("\n[the check timed out]");
            }
            if exit_ok && !ok {
                output.push_str(&format!("\n[exit 0, but the output does not contain the expected `{}`]", check.expect.as_deref().unwrap_or("")));
            }
            Ok(Ran {
                result: CheckResult {
                    name: check.name.clone(),
                    ok,
                    exit_code: run.exit_code,
                    output_tail: tail(&output),
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

/// Whether a check failed because its tool is not installed: `command not found` (exit 127), or
/// Python's `No module named x` for the module the command runs with `-m x`.
fn missing_tool(command: &str, exit_code: Option<i32>, output: &str) -> bool {
    if exit_code == Some(127) || output.contains("command not found") {
        return true;
    }
    match module_of(command) {
        Some(module) => output.contains(&format!("No module named {module}")) || output.contains(&format!("No module named '{module}'")),
        None => false,
    }
}

/// The first output line that says the sandbox refused an operation (the OS words it "Operation not
/// permitted", Go and others in lower case), shortened.
fn sandbox_refused(output: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.to_ascii_lowercase().contains("operation not permitted"))
        .map(|line| line.trim().chars().take(160).collect())
}

/// The module a `python -m <module>` command runs.
fn module_of(command: &str) -> Option<&str> {
    let mut words = command.split_whitespace();
    while let Some(word) = words.next() {
        if word == "-m" {
            return words.next();
        }
    }
    None
}

fn missing_tool_cause(command: &str, output: &str) -> String {
    match module_of(command) {
        Some(module) if output.contains("No module named") => format!("`{module}` is not installed in this project's environment"),
        _ => output.lines().find(|line| line.contains("command not found")).map(|line| line.trim().to_owned()).unwrap_or_else(|| "its command is not installed".to_owned()),
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
