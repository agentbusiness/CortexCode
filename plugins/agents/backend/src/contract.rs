//! Host-run contract checks. "Done" means the contract is green as checked by the host, never as
//! claimed by the model. Used by `agents.verify` and by the foreground DO route.

use crate::host::Host;
use cc_protocol::RpcError;
use cc_protocol::contracts::agents::{CheckResult, VerifyReport};
use cc_protocol::contracts::memory::{Contract, ContractCheck};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::tools::{RunResult, methods as tools_methods};
use serde_json::json;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// A check command may run a full test suite; this only guards against a hung process.
const CHECK_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// Characters of output kept in a report's `output_tail` (the full output is returned to the model
/// through `full_output`).
const TAIL_CHARS: usize = 6000;

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
                    result: CheckResult { name: check.name.clone(), ok: false, exit_code: None, output_tail: output.clone(), duration_ms: 0, indeterminate: false },
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
                },
                full_output: output,
            })
        }
        // A manual (or unknown) check cannot be decided by the host: INDETERMINATE, never passed.
        // The contract is then not green until someone other than the worker judges it.
        kind => {
            let output = format!("[indeterminate: a {kind} check is not machine-verifiable: {}]", check.spec);
            Ok(Ran {
                result: CheckResult { name: check.name.clone(), ok: false, exit_code: None, output_tail: output.clone(), duration_ms: 0, indeterminate: true },
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

/// Words of a shell command, with their byte ranges: split at blanks, quotes and shell punctuation
/// (mechanical; quoting is not interpreted, so a quoted `cd` or path is a word too).
fn words(command: &str) -> Vec<(usize, &str)> {
    let separator = |c: char| c.is_whitespace() || "'\"`;&|()<>{}$".contains(c);
    let mut out = Vec::new();
    let mut start = None;
    for (index, c) in command.char_indices() {
        match (separator(c), start) {
            (true, Some(from)) => {
                out.push((from, &command[from..index]));
                start = None;
            }
            (false, None) => start = Some(index),
            _ => {}
        }
    }
    if let Some(from) = start {
        out.push((from, &command[from..]));
    }
    out
}

/// Path-like parts of a word, with their offsets in it: the word, the value of `NAME=value` or
/// `--flag=value`, and each part of a `:`-separated list. URLs are not paths.
fn path_parts(word: &str) -> Vec<(usize, &str)> {
    if word.contains("://") {
        return Vec::new();
    }
    let (offset, value) = match word.find('=') {
        Some(at) => (at + 1, &word[at + 1..]),
        None => (0, word),
    };
    let mut parts = Vec::new();
    let mut from = 0;
    for part in value.split(':') {
        let looks_like_path = part.starts_with('/') || part.starts_with('~') || part == ".." || part.starts_with("../") || part.contains("/../") || part.ends_with("/..");
        if looks_like_path {
            parts.push((offset + from, part));
        }
        from += part.len() + 1;
    }
    parts
}

/// Device files a check may name.
const DEVICES: &[&str] = &["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/zero"];

/// `.` and `..` resolved lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// File identity (device, inode): equal for one directory under every name it has (symlinks,
/// macOS firmlinks such as `/System/Volumes/Data/private/var` for `/private/var`), so no spelling of
/// a path decides where it is.
fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|meta| (meta.dev(), meta.ino()))
}

/// The part of `path` below `root` (empty for the root itself), when `path` is inside it: the
/// nearest existing ancestor of `path` that is the same directory as `root` decides.
pub fn below(path: &Path, root: &Path) -> Option<PathBuf> {
    let path = normalize(path);
    let Some(root_id) = identity(root) else {
        let root = normalize(root);
        return path.strip_prefix(&root).ok().map(Path::to_path_buf);
    };
    let mut ancestor = path.clone();
    loop {
        if identity(&ancestor) == Some(root_id) {
            return path.strip_prefix(&ancestor).ok().map(Path::to_path_buf);
        }
        if !ancestor.pop() {
            return None;
        }
    }
}

fn expand(part: &str, root: &Path) -> PathBuf {
    match part.strip_prefix('~') {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest.trim_start_matches('/')),
        None if part.starts_with('/') => PathBuf::from(part),
        None => root.join(part),
    }
}

/// Why the host refuses to run `command` as a check in `root`, if it does. A check runs with the
/// root as its working directory, so it may not change directory (`cd`, `pushd`) nor name a path
/// outside the root (absolute, `~` or climbing with `..`), whatever its spelling: such a check
/// verifies something other than the work (a worker once moved its check into Homebrew's `bin`
/// and copied its files there to pass). Mechanical: words and file identities, no judgement.
pub fn refusal(command: &str, root: &str) -> Option<String> {
    let root = Path::new(root);
    if chains_alternatives(command) {
        return Some("it chains alternatives with `||` (a failing first command would hide behind the second); a check is one command whose own exit status decides".to_owned());
    }
    for (_, word) in words(command) {
        if matches!(word, "cd" | "pushd" | "popd" | "chdir") {
            return Some(format!("it changes directory (`{word}`); checks run in the workspace root ({}), so write it as a command run from there", root.display()));
        }
        for (_, part) in path_parts(word) {
            if DEVICES.contains(&part) {
                continue;
            }
            if below(&expand(part, root), root).is_none() {
                return Some(format!("it names `{part}`, outside the workspace root ({}); a check may only use paths inside it", root.display()));
            }
        }
    }
    None
}

/// Whether a command chains alternatives with `||` outside quotes (mechanical scan; `||` inside a
/// quoted string, e.g. a Python expression, is the program's business).
pub fn chains_alternatives(command: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut previous = '\0';
    for c in command.chars() {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '|' && previous == '|' => return true,
            None => {}
        }
        previous = if quote.is_some() { '\0' } else { c };
    }
    false
}

/// A check command as the root runs it: a leading `cd <dir> &&` (or `;`) dropped and absolute
/// paths rewritten relative, when `<dir>` or the path is inside one of `dirs` (the directory the
/// host runs the check in anyway, under any spelling: compared by file identity, not by string).
/// A check naming one copy's absolute path would otherwise verify that copy wherever it runs
/// (e.g. after a merge). Anything else is left for [`refusal`] to judge.
pub fn rooted(command: &str, dirs: &[&str]) -> String {
    let dirs: Vec<&Path> = dirs.iter().filter(|dir| !dir.is_empty()).map(Path::new).collect();
    let inside = |part: &str| -> Option<String> {
        let path = Path::new(part.trim_matches(|c| c == '\'' || c == '"'));
        if !path.is_absolute() {
            return None;
        }
        dirs.iter().find_map(|dir| below(path, dir)).map(|rest| if rest.as_os_str().is_empty() { ".".to_owned() } else { rest.display().to_string() })
    };
    let mut command = command.trim().to_owned();
    if let Some(rest) = command.strip_prefix("cd ") {
        let rest = rest.trim_start();
        let end = rest.find("&&").or_else(|| rest.find(';'));
        if let Some(end) = end
            && inside(rest[..end].trim().trim_end_matches('/')).is_some()
        {
            let after = &rest[end..];
            command = after.strip_prefix("&&").or_else(|| after.strip_prefix(';')).unwrap_or(after).trim().to_owned();
        }
    }
    let mut out = String::with_capacity(command.len());
    let mut copied = 0;
    for (at, word) in words(&command) {
        for (offset, part) in path_parts(word) {
            if let Some(relative) = inside(part) {
                let from = at + offset;
                out.push_str(&command[copied..from]);
                out.push_str(&relative);
                copied = from + part.len();
            }
        }
    }
    out.push_str(&command[copied..]);
    out
}

/// Contract checks from decided check commands (command kind; success = exit status 0).
pub fn contract_from(checks: &[String]) -> Contract {
    let mut seen = std::collections::HashSet::new();
    Contract {
        checks: checks
            .iter()
            // A leading shell prompt marker (`$ `) is never part of a command (mechanical).
            .map(|command| command.trim().strip_prefix("$ ").unwrap_or(command.trim()).trim())
            .filter(|command| !command.is_empty() && seen.insert(command.to_owned()))
            .enumerate()
            .map(|(index, command)| ContractCheck {
                name: format!("check {}", index + 1),
                kind: "command".into(),
                spec: command.to_owned(),
                expect: None,
            })
            .collect(),
    }
}

/// The command checks of a contract.
pub fn commands(contract: &Contract) -> Vec<String> {
    contract.checks.iter().filter(|check| check.kind == "command").map(|check| check.spec.clone()).collect()
}
