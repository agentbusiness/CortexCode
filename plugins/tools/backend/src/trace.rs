//! Traces of the commands `run` runs, recorded by the CortexTerm on this Mac (its `system.trace.*`,
//! lent over the mesh; docs/STANDARD-CHANGES.md). While a command runs, CortexTerm follows the
//! session shell's process tree: CPU, memory, GPU, disk writes, the processes that came and went,
//! files, connections and ports. A command that ran long enough gets one line in its result and a
//! `trace://<id>` the model reads for the summary. Without a CortexTerm on this Mac nothing
//! changes: every call here gives up quietly.

use crate::host::Host;
use crate::uris::UriError;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Shorter commands are not worth a line: a process scan every 4 s would have seen little.
pub const WORTH_MS: u64 = 4_000;
const ASK: Duration = Duration::from_secs(3);

/// The CortexTerm on this Mac: a local member whose card has a `system` plugin.
async fn recorder(host: &dyn Host) -> Option<String> {
    let peers = tokio::time::timeout(ASK, host.call("mesh.peers", json!({}))).await.ok()?.ok()?;
    peers["peers"].as_array()?.iter().find_map(|peer| {
        let local = peer["member"]["local"].as_bool().unwrap_or(false) && peer["reachable"].as_bool().unwrap_or(false);
        let system = peer["card"]["plugins"].as_array().is_some_and(|plugins| plugins.iter().any(|p| p["id"] == "system" && p["active"].as_bool().unwrap_or(true)));
        (local && system).then(|| peer["member"]["node"].as_str().map(str::to_owned)).flatten()
    })
}

async fn ask(host: &dyn Host, node: &str, method: &str, params: Value, limit: Duration) -> Option<Value> {
    let call = host.call("mesh.call", json!({"node": node, "method": method, "params": params}));
    tokio::time::timeout(limit, call).await.ok()?.ok()
}

/// A trace being recorded for one command.
pub struct Recording {
    node: String,
    pub id: String,
}

/// Start tracing the session shell `pid` (its whole tree) for one command.
pub async fn start(host: &dyn Host, pid: i32, command: &str) -> Option<Recording> {
    let node = recorder(host).await?;
    let label: String = command.chars().take(200).collect();
    let info = ask(host, &node, "system.trace.start", json!({"pid": pid, "label": label, "asked_by": "cortexcode"}), ASK).await?;
    Some(Recording { node, id: info["id"].as_str()?.to_owned() })
}

/// Start a trace once the session's shell is up (the run starts it): the task's result is the
/// recording, if any.
pub fn begin(host: Arc<dyn Host>, groups: Arc<crate::guard::ShellGroups>, session: String, command: String) -> tokio::task::JoinHandle<Option<Recording>> {
    tokio::spawn(async move {
        for _ in 0..10 {
            if let Some(pid) = groups.pid(&session) {
                return start(host.as_ref(), pid, &command).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    })
}

/// The command ended: stop the trace. One line for the result when it ran for [`WORTH_MS`] or
/// more; a shorter one's trace is deleted.
pub async fn finish(host: &dyn Host, recording: Recording, duration_ms: u64, workspace: &Path) -> Option<String> {
    let Recording { node, id } = recording;
    if duration_ms < WORTH_MS {
        ask(host, &node, "system.trace.delete", json!({"id": id}), ASK).await;
        return None;
    }
    ask(host, &node, "system.trace.stop", json!({"id": id}), ASK).await?;
    let summary = ask(host, &node, "system.trace.summary", json!({"id": id}), Duration::from_secs(10)).await?;
    Some(line(&summary, &id, workspace))
}

/// `[trace://t-…: peak CPU 380%, 1.7 GB; ran rustc ×4, cargo; wrote target. read it for how it used the machine]`
fn line(summary: &Value, id: &str, workspace: &Path) -> String {
    let mut parts = Vec::new();
    let metric = |name: &str| summary["metrics"].as_array().and_then(|list| list.iter().find(|m| m["name"] == name)).and_then(|m| m["max"].as_f64());
    let mut peak = Vec::new();
    if let Some(cpu) = metric("cpu").filter(|cpu| *cpu > 0.0) {
        peak.push(format!("peak CPU {cpu:.0}%"));
    }
    if let Some(memory) = metric("memory").filter(|memory| *memory > 0.0) {
        peak.push(if memory >= 1e9 { format!("{:.1} GB", memory / 1e9) } else { format!("{:.0} MB", memory / 1e6) });
    }
    if let Some(gpu) = metric("gpu").filter(|gpu| *gpu >= 1.0) {
        peak.push(format!("GPU {gpu:.0}%"));
    }
    if !peak.is_empty() {
        parts.push(peak.join(", "));
    }
    let programs: Vec<String> = summary["processes"]
        .as_array()
        .into_iter()
        .flatten()
        .take(4)
        .filter_map(|p| {
            let name = p["name"].as_str()?;
            let count = p["count"].as_u64().unwrap_or(1);
            Some(if count > 1 { format!("{name} ×{count}") } else { name.to_owned() })
        })
        .collect();
    if !programs.is_empty() {
        parts.push(format!("ran {}", programs.join(", ")));
    }
    let wrote: Vec<String> = summary["touched"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["how"] == "wrote")
        .take(3)
        .filter_map(|t| t["name"].as_str().map(|name| crate::util::relative_paths(name, workspace)))
        .collect();
    if !wrote.is_empty() {
        parts.push(format!("wrote {}", wrote.join(", ")));
    }
    format!("[trace://{id}: {}. Read it for how the command used the machine]", parts.join("; "))
}

/// `trace://<id>`: the trace's summary, as CortexTerm wrote it for a model.
pub async fn read(host: &dyn Host, id: &str, workspace: &Path) -> Result<String, UriError> {
    let id = id.trim_matches('/');
    let Some(node) = recorder(host).await else {
        return Err(UriError::new("unavailable", "no CortexTerm on this Mac: traces are recorded and kept there"));
    };
    let call = host.call("mesh.call", json!({"node": node, "method": "system.trace.summary", "params": {"id": id}}));
    match tokio::time::timeout(Duration::from_secs(10), call).await {
        Ok(Ok(summary)) => Ok(crate::util::relative_paths(summary["text"].as_str().unwrap_or(""), workspace)),
        Ok(Err(error)) => Err(UriError::new("not_found", format!("trace://{id}: {}", error.message))),
        Err(_) => Err(UriError::new("unavailable", format!("trace://{id}: CortexTerm did not answer"))),
    }
}
