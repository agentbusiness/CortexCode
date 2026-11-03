//! What this plugin shares (`cc_protocol::contracts::service`, docs/STANDARD-CHANGES.md): the code
//! audit's line scan and dependency check as the `tools.security` service, so the CortexTerm on
//! this Mac can check a script it fetched, an MCP server's package or a launch agent's program
//! with the same attack classes and lockfile parsers the audit uses. Both read files of this Mac,
//! so they answer only apps on this machine. It is for apps' own checks, never a model tool
//! (`Audience::Apps`): CortexCode's model has the audit as a skill.
//!
//! The work is the audit's own script (`skills-security`'s `secaudit.py`, `scan --json` and
//! `deps --json`): one implementation of the classes and parsers, never a second copy.

use cc_protocol::contracts::service::{Audience, Caller, Share, ShareList, SharedMethod};
use cc_protocol::contracts::tools::methods;
use cc_protocol::{RpcError, codes};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const SERVICE: &str = "tools.security";
/// A scan of a few files takes seconds; a large project's dependencies a minute.
const LIMIT: Duration = Duration::from_secs(120);
/// The answer's largest size (a scan of many files is cut by the script's own caps first).
const ANSWER_BYTES: usize = 4 << 20;

fn method(name: &str, description: &str, params: Value) -> SharedMethod {
    SharedMethod { method: name.into(), description: description.into(), params }
}

pub fn shares() -> ShareList {
    let security = Share {
        service: SERVICE.into(),
        version: 1,
        title: "Code security checks".into(),
        description: "The code audit's checks: dangerous code patterns in files, and known vulnerabilities in a project's locked dependencies.".into(),
        methods: vec![
            method(
                methods::SECURITY_SCAN,
                "Scan files for dangerous code patterns (injection, secrets, unsafe deserialization, AI-agent configs ...). Answers each hit with its class, file, line and why; secret values are masked.",
                json!({
                    "type": "object",
                    "properties": {
                        "paths": {"type": "array", "items": {"type": "string"}, "description": "Absolute paths of files or folders on this machine."},
                        "classes": {"type": "array", "items": {"type": "string"}, "description": "Only these attack classes (all that apply when absent)."}
                    },
                    "required": ["paths"]
                }),
            ),
            method(
                methods::SECURITY_DEPS,
                "Check a project's locked dependencies against OSV. Answers each vulnerable package with its advisories, the version that fixes them and the command that upgrades it.",
                json!({
                    "type": "object",
                    "properties": {"root": {"type": "string", "description": "The project's absolute folder on this machine."}},
                    "required": ["root"]
                }),
            ),
        ],
        audience: Audience::Apps,
    };
    ShareList { shares: vec![security] }
}

/// `tools.security.scan.get` and `tools.security.deps.get`.
pub async fn security(method: &str, params: Value) -> Result<Value, RpcError> {
    // A call through the mesh carries who asked; one from another computer would read this Mac's
    // files, so only apps on this machine are answered. A call inside this app has no caller.
    if let Some(caller) = params.get("caller").filter(|caller| !caller.is_null()) {
        let caller: Caller = serde_json::from_value(caller.clone()).map_err(|error| RpcError::invalid_params(format!("caller: {error}")))?;
        if !caller.local {
            return Err(RpcError::new(codes::CONTRACT_VIOLATION, format!("`{method}` reads files of this machine, so it answers only apps on this machine")));
        }
    }
    let script = script().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "the code audit (skills-security) is not installed".to_owned()))?;
    let mut args: Vec<String> = Vec::new();
    match method {
        methods::SECURITY_SCAN => {
            let paths: Vec<String> = params.get("paths").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
            if paths.is_empty() {
                return Err(RpcError::invalid_params("`paths` needs at least one absolute path"));
            }
            for path in &paths {
                if !Path::new(path).is_absolute() || !Path::new(path).exists() {
                    return Err(RpcError::invalid_params(format!("`{path}` is not an absolute path that exists here")));
                }
            }
            args.extend(["scan".to_owned(), "--json".to_owned()]);
            if let Some(classes) = params.get("classes").and_then(Value::as_array) {
                let classes: Vec<&str> = classes.iter().filter_map(Value::as_str).filter(|c| c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-')).collect();
                if !classes.is_empty() {
                    args.extend(["--classes".to_owned(), classes.join(",")]);
                }
            }
            args.push("--".to_owned());
            args.extend(paths);
        }
        _ => {
            let root = params.get("root").and_then(Value::as_str).unwrap_or_default();
            if !Path::new(root).is_absolute() || !Path::new(root).is_dir() {
                return Err(RpcError::invalid_params(format!("`root` ({root}) is not an absolute folder that exists here")));
            }
            args.extend(["deps".to_owned(), "--json".to_owned(), "--root".to_owned(), root.to_owned()]);
        }
    }
    let mut command = tokio::process::Command::new("python3");
    command.arg("-I").arg(&script).args(&args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let output = match tokio::time::timeout(LIMIT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(RpcError::new(codes::UNAVAILABLE, format!("python3 could not start: {error}"))),
        Err(_) => return Err(RpcError::new(codes::TIMEOUT, format!("the check did not finish in {} s", LIMIT.as_secs()))),
    };
    if output.stdout.len() > ANSWER_BYTES {
        return Err(RpcError::internal(format!("the check's answer is larger than {} MiB", ANSWER_BYTES >> 20)));
    }
    match serde_json::from_slice::<Value>(&output.stdout) {
        // A usage error answers `{"error": ...}` with a failing exit.
        Ok(answer) if !output.status.success() || answer.get("error").is_some() => {
            let error = answer.get("error").and_then(Value::as_str).unwrap_or("the check failed").to_owned();
            Err(RpcError::invalid_params(error))
        }
        Ok(answer) if answer.is_object() => Ok(answer),
        _ => {
            let error: String = String::from_utf8_lossy(&output.stderr).lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("no answer").chars().take(300).collect();
            Err(RpcError::internal(format!("the check failed: {error}")))
        }
    }
}

/// The audit script in the live folder (`<home>/live/plugins/skills-security/data/scripts`).
fn script() -> Option<PathBuf> {
    let live = std::env::var_os("CORTEX_PLUGIN_LIVE").map(PathBuf::from).and_then(|own| own.parent().map(Path::to_path_buf));
    let live = live.or_else(|| std::env::var_os("CORTEX_HOME").map(|home| PathBuf::from(home).join("live").join("plugins")))?;
    let script = live.join("skills-security").join("data").join("scripts").join("secaudit.py");
    script.is_file().then_some(script)
}
