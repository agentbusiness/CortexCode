//! What a model reads from a service's answers: short lines with the facts that decide the next
//! step (state, exit code, time, the machine's peaks, the first error) and how to read more. A
//! provider that wrote `Job.text` or a summary's `text` is shown as it wrote it.

use cc_protocol::contracts::service::{Job, Moment};
use serde_json::Value;
use std::path::Path;

/// `2m04s`, `42 s`, `850 ms`.
pub fn duration(ms: u64) -> String {
    match ms {
        0..1_000 => format!("{ms} ms"),
        1_000..60_000 => format!("{} s", ms / 1_000),
        _ => format!("{}m{:02}s", ms / 60_000, ms / 1_000 % 60),
    }
}

/// `1.7 GB`, `420 MB`.
pub fn bytes(n: f64) -> String {
    if n >= 1e9 { format!("{:.1} GB", n / 1e9) } else { format!("{:.0} MB", n / 1e6) }
}

/// A path under `root` as the model knows it (relative), others as they are.
fn shown(path: &str, root: Option<&Path>) -> String {
    match root.and_then(|root| Path::new(path).strip_prefix(root).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => ".".into(),
        Some(rest) => rest.to_string_lossy().into_owned(),
        None => path.to_owned(),
    }
}

/// A job, for a model: the provider's own text when it wrote one, otherwise its state, exit code,
/// time and last line.
pub fn job(job: &Job) -> String {
    if !job.text.trim().is_empty() {
        return job.text.trim_end().to_owned();
    }
    let mut first = match job.state.as_str() {
        "done" | "failed" => match job.exit_code {
            Some(code) => format!("{}: exit {code} in {}", job.state, duration(job.duration_ms)),
            None => format!("{} in {}", job.state, duration(job.duration_ms)),
        },
        "running" | "waiting" => format!("{} for {}", job.state, duration(job.duration_ms)),
        "lost" => "lost: the provider stopped while it ran; its result is unknown".to_owned(),
        "refused" => match &job.needs_approval {
            Some(why) => format!("refused ({}): {}", why.risk, why.reason),
            None => "refused".to_owned(),
        },
        other => other.to_owned(),
    };
    if let Some(line) = job.trace_line.as_deref().filter(|line| !line.is_empty()) {
        first.push_str("; ");
        first.push_str(line);
    }
    match job.tail.iter().rev().find(|line| !line.trim().is_empty()) {
        Some(last) => format!("{first}\nlast line: {}", last.trim()),
        None => first,
    }
}

/// One line from a trace summary (`system.trace.read {size: summary}`): peak CPU and memory, the
/// programs that ran most, what it wrote. Paths under `root` are shown relative.
pub fn summary_line(summary: &Value, root: Option<&Path>) -> String {
    let metric = |name: &str| summary["metrics"].as_array().and_then(|list| list.iter().find(|m| m["name"] == name)).and_then(|m| m["max"].as_f64());
    let mut parts = Vec::new();
    let mut peak = Vec::new();
    if let Some(cpu) = metric("cpu").filter(|cpu| *cpu > 0.0) {
        peak.push(format!("peak CPU {cpu:.0}%"));
    }
    if let Some(memory) = metric("memory").filter(|memory| *memory > 0.0) {
        peak.push(bytes(memory));
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
    let wrote: Vec<String> = summary["touched"].as_array().into_iter().flatten().filter(|t| t["how"] == "wrote").take(3).filter_map(|t| t["name"].as_str().map(|name| shown(name, root))).collect();
    if !wrote.is_empty() {
        parts.push(format!("wrote {}", wrote.join(", ")));
    }
    parts.join("; ")
}

/// A trace summary, for a model: the provider's `text`, its paths under `root` relative.
pub fn summary(summary: &Value, root: Option<&Path>) -> String {
    let text = summary["text"].as_str().unwrap_or("");
    if text.trim().is_empty() {
        return summary_line(summary, root);
    }
    match root.map(|root| root.to_string_lossy().into_owned()).filter(|root| root.len() > 1) {
        Some(root) => text.replace(&format!("{root}/"), "").replace(&root, "."),
        None => text.to_owned(),
    }
}

/// A moment of a traced run (`at`), for a model: the provider's text, or what ran, the memory, the
/// files open and the last output lines.
pub fn moment(moment: &Moment, root: Option<&Path>) -> String {
    if !moment.text.trim().is_empty() {
        return moment.text.trim_end().to_owned();
    }
    let mut out = String::new();
    let mut processes: Vec<String> = moment.processes.iter().take(8).map(|p| format!("{} (pid {}, CPU {:.0}%, {})", p.name, p.pid, p.cpu, bytes(p.memory as f64))).collect();
    if processes.is_empty() {
        processes.push("nothing".into());
    }
    out.push_str(&format!("running: {}\n", processes.join(", ")));
    out.push_str(&format!("memory in all: {}\n", bytes(moment.memory_total as f64)));
    if !moment.open.is_empty() {
        let files: Vec<String> = moment.open.iter().take(10).map(|f| format!("{} ({})", shown(&f.path, root), f.mode)).collect();
        out.push_str(&format!("files open: {}\n", files.join(", ")));
    }
    if !moment.connections.is_empty() {
        out.push_str(&format!("network: {}\n", moment.connections.join(", ")));
    }
    if !moment.last_output.is_empty() {
        out.push_str("last output before then:\n");
        for line in &moment.last_output {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_end().to_owned()
}
